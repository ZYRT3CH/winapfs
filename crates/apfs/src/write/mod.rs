//! Writing to APFS containers. EXPERIMENTAL, and limited to disk image files
//! (see [`ImageFile`]): every step is checked against independent checkers on
//! throwaway images before it may ever touch a real disk.
//!
//! APFS never overwrites live metadata: a transaction writes new copies of
//! what it changes, then a new checkpoint (checkpoint-map blocks plus a new
//! container superblock) in the checkpoint rings. Until that superblock is
//! written the previous checkpoint stays the newest valid one, so a crash
//! mid-transaction loses the transaction, not the container.

mod fs;
mod space;
mod tree;
mod vol;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::bytes::u64_at;
use crate::checksum;
use crate::dev::{BlockDevice, DiskFile, FileDevice, ImageFile, Overlay, Shared, WriteDevice};
use crate::error::{corrupt, unsupported, Error, Result};
use crate::fs::{Inode, Volume};
use crate::gpt;
use crate::nx::{CheckpointMapping, Container};
use crate::obj::{OBJ_PHYSICAL, OBJECT_TYPE_CHECKPOINT_MAP};
use space::Space;
use tree::{omap_cmp, omap_drop, omap_put, PhysReader, Tree};
use vol::{VolEdit, VolState};
pub use vol::{NewKind, Times};

const CHECKPOINT_MAP_LAST: u32 = 0x1;
/// Free space that writes and creates leave for the metadata their commit
/// writes (new tree nodes, the space manager's blocks).
const METADATA_RESERVE: u64 = 16 << 20;
/// `NX_RESERVED_2`: "remove it when modifying the container".
const NX_RESERVED_2: u64 = 0x2;
/// Shown in a volume's `apfs_modified_by` history.
const MODIFIED_BY: &str = concat!("winapfs ", env!("CARGO_PKG_VERSION"));

/// Sets an object's transaction id.
pub(crate) fn set_xid(obj: &mut [u8], xid: u64) {
    obj[16..24].copy_from_slice(&xid.to_le_bytes());
}

/// Stores a fresh checksum in an object's header.
pub(crate) fn seal(obj: &mut [u8]) {
    let c = checksum::compute(obj);
    obj[0..8].copy_from_slice(&c.to_le_bytes());
}

pub(crate) fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LOG: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

/// Turns on timing messages for every transaction, not only slow ones.
pub fn set_trace(on: bool) {
    TRACE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Where timing and diagnostic messages go (standard error unless set).
pub fn set_log(f: fn(&str)) {
    let _ = LOG.set(f);
}

fn tracing() -> bool {
    TRACE.load(std::sync::atomic::Ordering::Relaxed)
}

fn log(msg: &str) {
    match LOG.get() {
        Some(f) => f(msg),
        None => eprintln!("{msg}"),
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// Opens a fresh reader on the device a [`Writer`] writes to.
type OpenReader = Box<dyn Fn() -> Result<Box<dyn BlockDevice>> + Send + Sync>;

/// An APFS container open for writing: in an image file, or a real disk
/// seen through an [`Overlay`] that keeps the writes in memory.
pub struct Writer {
    c: Container,
    dev: Box<dyn WriteDevice>,
    reader: OpenReader,
}

/// One transaction: block allocations, new object copies and superblock
/// changes, all applied together by [`Writer::commit`].
pub struct Txn {
    pub xid: u64,
    space: Space,
    writes: Vec<(u64, Vec<u8>)>,
    nx_omap_oid: Option<u64>,
    /// `nx_next_oid`: ids for new virtual objects, such as tree nodes.
    next_oid: u64,
}

/// Byte offset of the APFS container in an image: its GPT partition, or 0.
fn container_offset(path: &Path) -> Result<u64> {
    let dev = FileDevice::open(path)?;
    Ok(match gpt::read_partitions(&dev)? {
        Some(parts) => parts.iter().find(|p| p.is_apfs()).ok_or_else(|| corrupt("no APFS partition in the image"))?.offset,
        None => 0,
    })
}

impl Writer {
    pub fn open(path: &Path) -> Result<Self> {
        let base = container_offset(path)?;
        let dev = ImageFile::open_rw(path)?.with_base(base)?;
        let path = path.to_path_buf();
        let reader: OpenReader = Box::new(move || Ok(Box::new(FileDevice::open(&path)?.with_base(base)?)));
        let c = Container::open(reader()?)?;
        Ok(Self { c, dev: Box::new(dev), reader })
    }

    /// Opens the container on a real disk for writing, from byte `base` of
    /// `path`. Refused unless the container's UUID is `expected_uuid`, so a
    /// renumbered disk can never be written by mistake.
    pub fn open_real_disk(path: &Path, base: u64, expected_uuid: [u8; 16]) -> Result<Self> {
        let read_path = path.to_path_buf();
        let reader: OpenReader = Box::new(move || Ok(Box::new(FileDevice::open(&read_path)?.with_base(base)?)));
        let c = Container::open(reader()?)?;
        if c.sb.uuid != expected_uuid {
            return Err(unsupported(format!(
                "container {} is not the one named for writing ({})",
                crate::format_uuid(&c.sb.uuid),
                crate::format_uuid(&expected_uuid)
            )));
        }
        let dev = DiskFile::open_rw(path, base)?;
        Ok(Self { c, dev: Box::new(dev), reader })
    }

    /// A writer for the container on `base` (a read-only device) whose
    /// writes stay in the returned overlay: `base` is never written.
    pub fn rehearsal(base: FileDevice) -> Result<(Self, Arc<Overlay>)> {
        let bs = Container::open(Box::new(base.try_clone()?))?.block_size() as u64;
        let ov = Arc::new(Overlay::new(Box::new(base), bs));
        let shared = Arc::clone(&ov);
        let reader: OpenReader = Box::new(move || Ok(Box::new(Shared(Arc::clone(&shared)))));
        let c = Container::open(reader()?)?;
        Ok((Self { c, dev: Box::new(Shared(Arc::clone(&ov))), reader }, ov))
    }

    /// The committed state, as a reader sees it.
    pub fn container(&self) -> &Container {
        &self.c
    }

    fn reload(&mut self) -> Result<()> {
        self.c = Container::open((self.reader)()?)?;
        Ok(())
    }

    /// Starts a transaction. Blocks that earlier transactions freed become
    /// reusable now.
    pub fn begin(&self) -> Result<Txn> {
        let start = Instant::now();
        let xid = self.c.sb.next_xid;
        let mut space = Space::load(&self.c, xid)?;
        space.release_queued(&self.c)?;
        if start.elapsed().as_secs_f64() > 0.5 || tracing() {
            log(&format!("winapfs: starting transaction {xid} took {:.2} s", start.elapsed().as_secs_f64()));
        }
        Ok(Txn { xid, space, writes: Vec::new(), nx_omap_oid: None, next_oid: self.c.sb.next_oid })
    }

    /// Writes a new checkpoint that records the current state unchanged.
    pub fn commit_empty(&mut self) -> Result<u64> {
        let t = self.begin()?;
        self.commit(t)
    }

    /// Renames volume `index` (a small change that still exercises the
    /// object map, the space manager and the free queues).
    pub fn rename_volume(&mut self, index: usize, name: &str) -> Result<u64> {
        if name.len() >= 256 || name.contains('\0') {
            return Err(unsupported("volume names of 256 bytes or more"));
        }
        let mut t = self.begin()?;
        let oid = *self.c.sb.fs_oids.get(index).ok_or_else(|| corrupt(format!("no volume {index}")))?;
        let old = self.c.omap_lookup(oid)?;
        let mut apsb = self.c.raw_block(old)?;
        if !checksum::verify(&apsb) {
            return Err(corrupt("volume superblock checksum"));
        }
        apsb[704..960].fill(0);
        apsb[704..704 + name.len()].copy_from_slice(name.as_bytes());
        stamp_modified_by(&mut apsb, t.xid);
        set_xid(&mut apsb, t.xid);
        seal(&mut apsb);
        let new = t.space.alloc(&self.c)?;
        t.writes.push((new, apsb));
        t.space.free(old);
        let (omap, _, _) = self.omap_update(&mut t, self.c.sb.omap_oid, &[(oid, new)], &[])?;
        t.nx_omap_oid = Some(omap);
        self.commit(t)
    }

    /// Writes a volume's changes: the records of files being written, its
    /// file-system and extent-reference trees, its object map and a new copy
    /// of its superblock.
    fn finish_volume(&self, t: &mut Txn, v: &Volume, mut st: VolState) -> Result<()> {
        if t.nx_omap_oid.is_some() {
            return Err(unsupported("changing two volumes in one transaction"));
        }
        VolEdit { v, st: &mut st }.flush_files(&*self.dev)?;
        let VolState { tree, extref, mut apsb, data_alloced, data_freed, .. } = st;
        let (mut alloced, mut freed) = (data_alloced, data_freed);
        if tree.changed() {
            let f = tree.flush(&self.c, &mut t.space, &mut t.writes, t.xid, &mut t.next_oid)?;
            if f.root != v.sb.root_tree_oid {
                return Err(corrupt("the file-system tree root changed its object id"));
            }
            let (omap, a, fr) = self.omap_update(t, v.sb.omap_oid, &f.moved, &f.removed)?;
            put_u64(&mut apsb, 128, omap); // apfs_omap_oid
            alloced += f.allocated + a;
            freed += f.freed + fr;
        }
        if let Some(x) = extref.filter(Tree::changed) {
            let mut no_oids = 0;
            let f = x.flush(&self.c, &mut t.space, &mut t.writes, t.xid, &mut no_oids)?;
            put_u64(&mut apsb, 144, f.root); // apfs_extentref_tree_oid
            alloced += f.allocated;
            freed += f.freed;
        }
        // Blocks the volume uses (data, trees, object map), and running totals.
        let (used, total_alloced, total_freed) = (u64_at(&apsb, 88)?, u64_at(&apsb, 224)?, u64_at(&apsb, 232)?);
        put_u64(&mut apsb, 88, used + alloced - freed); // apfs_fs_alloc_count
        put_u64(&mut apsb, 224, total_alloced + alloced); // apfs_total_blocks_alloced
        put_u64(&mut apsb, 232, total_freed + freed); // apfs_total_blocks_freed
        stamp_modified_by(&mut apsb, t.xid);
        set_xid(&mut apsb, t.xid);
        seal(&mut apsb);
        let old = self.c.omap_lookup(v.sb.hdr.oid)?;
        let new = t.space.alloc(&self.c)?;
        t.writes.push((new, apsb));
        t.space.free(old);
        let (omap, _, _) = self.omap_update(t, self.c.sb.omap_oid, &[(v.sb.hdr.oid, new)], &[])?;
        t.nx_omap_oid = Some(omap);
        Ok(())
    }

    /// Applies changes to the object map whose `omap_phys_t` is at
    /// `omap_paddr`: points each `(oid, paddr)` of `puts` at its new block
    /// and removes the objects in `drops`. Copy-on-write: the tree's nodes and
    /// the object map itself move. Returns the object map's new address and
    /// the blocks allocated and freed.
    fn omap_update(&self, t: &mut Txn, omap_paddr: u64, puts: &[(u64, u64)], drops: &[u64]) -> Result<(u64, u64, u64)> {
        let mut phys = self.c.raw_block(omap_paddr)?;
        if !checksum::verify(&phys) {
            return Err(corrupt("object map checksum"));
        }
        let rd = PhysReader(&self.c);
        let mut tree = Tree::open(&rd, u64_at(&phys, 48)?, omap_cmp)?;
        for &oid in drops {
            omap_drop(&mut tree, &rd, oid)?;
        }
        for &(oid, paddr) in puts {
            omap_put(&mut tree, &rd, oid, t.xid, paddr, self.c.block_size())?;
        }
        let mut no_oids = 0;
        let f = tree.flush(&self.c, &mut t.space, &mut t.writes, t.xid, &mut no_oids)?;
        let new_phys = t.space.alloc(&self.c)?;
        put_u64(&mut phys, 8, new_phys);
        set_xid(&mut phys, t.xid);
        put_u64(&mut phys, 48, f.root); // om_tree_oid
        seal(&mut phys);
        t.writes.push((new_phys, phys));
        t.space.free(omap_paddr);
        Ok((new_phys, f.allocated + 1, f.freed + 1))
    }

    /// Commits a transaction: writes its blocks, then a new checkpoint.
    /// Returns the transaction id.
    pub fn commit(&mut self, t: Txn) -> Result<u64> {
        let t_start = Instant::now();
        let Txn { xid, space, mut writes, nx_omap_oid, next_oid } = t;
        let mut next_oid = next_oid;
        let bitmap_reads = space.bitmap_reads;
        let (objects, dropped) = space.finish(&mut writes, &mut next_oid)?;
        // New ephemeral objects (more free-queue nodes) join the checkpoint map.
        let added: Vec<CheckpointMapping> = objects
            .iter()
            .filter(|o| !self.c.checkpoint_map.iter().any(|m| m.oid == o.oid))
            .map(|o| CheckpointMapping {
                obj_type: o.obj_type,
                subtype: o.subtype,
                size: self.c.block_size(),
                fs_oid: 0,
                oid: o.oid,
                paddr: 0,
            })
            .collect();
        let ephemeral: HashMap<u64, Vec<u8>> = objects.into_iter().map(|o| (o.oid, o.data)).collect();

        let sb = &self.c.sb;
        if sb.xp_desc_blocks & 0x8000_0000 != 0 || sb.xp_data_blocks & 0x8000_0000 != 0 {
            return Err(unsupported("non-contiguous checkpoint areas"));
        }
        let bs = self.c.block_size() as u64;
        let (desc_base, desc_n) = (sb.xp_desc_base, sb.xp_desc_blocks as u64);
        let (data_base, data_n) = (sb.xp_data_base, sb.xp_data_blocks as u64);

        // Ephemeral objects go into the data ring right after the current
        // checkpoint's, so the current checkpoint stays intact.
        let data_start = sb.xp_data_next as u64 % data_n;
        let mut data_pos = data_start;
        let mut eph_writes: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut mappings: Vec<CheckpointMapping> = Vec::new();
        for m in self.c.checkpoint_map.iter().filter(|m| !dropped.contains(&m.oid)).chain(&added) {
            let mut obj = match ephemeral.get(&m.oid) {
                Some(updated) => updated.clone(),
                None => {
                    let mut o = self.c.ephemeral(m.oid)?;
                    set_xid(&mut o, xid);
                    seal(&mut o);
                    o
                }
            };
            obj.resize((obj.len() as u64).div_ceil(bs) as usize * bs as usize, 0);
            let first = data_base + data_pos % data_n;
            for block in obj.chunks(bs as usize) {
                eph_writes.push((data_base + data_pos % data_n, block.to_vec()));
                data_pos += 1;
            }
            mappings.push(CheckpointMapping { paddr: first, ..*m });
        }
        let data_len = data_pos - data_start;
        if data_len + sb.xp_data_len as u64 > data_n {
            return Err(unsupported("checkpoint data area too small for another checkpoint"));
        }

        let entries_per_block = (bs as usize - 40) / 40;
        if mappings.len() > entries_per_block {
            return Err(unsupported("checkpoints that need several checkpoint-map blocks"));
        }
        if 2 + sb.xp_desc_len as u64 > desc_n {
            return Err(unsupported("checkpoint descriptor area too small for another checkpoint"));
        }
        let cpm_pos = sb.xp_desc_next as u64 % desc_n;
        let sb_pos = (cpm_pos + 1) % desc_n;
        let cpm_paddr = desc_base + cpm_pos;
        let mut cpm = vec![0u8; bs as usize];
        put_u64(&mut cpm, 8, cpm_paddr); // a physical object's id is its address
        put_u64(&mut cpm, 16, xid);
        put_u32(&mut cpm, 24, OBJ_PHYSICAL | OBJECT_TYPE_CHECKPOINT_MAP);
        put_u32(&mut cpm, 32, CHECKPOINT_MAP_LAST);
        put_u32(&mut cpm, 36, mappings.len() as u32);
        for (i, m) in mappings.iter().enumerate() {
            let e = 40 + i * 40;
            put_u32(&mut cpm, e, m.obj_type);
            put_u32(&mut cpm, e + 4, m.subtype);
            put_u32(&mut cpm, e + 8, m.size);
            put_u64(&mut cpm, e + 16, m.fs_oid);
            put_u64(&mut cpm, e + 24, m.oid);
            put_u64(&mut cpm, e + 32, m.paddr);
        }
        seal(&mut cpm);

        let mut nsb = sb.raw.clone();
        set_xid(&mut nsb, xid);
        put_u64(&mut nsb, 88, next_oid); // nx_next_oid
        put_u64(&mut nsb, 96, xid + 1); // nx_next_xid
        put_u32(&mut nsb, 128, ((sb_pos + 1) % desc_n) as u32); // nx_xp_desc_next
        put_u32(&mut nsb, 132, (data_pos % data_n) as u32); // nx_xp_data_next
        put_u32(&mut nsb, 136, cpm_pos as u32); // nx_xp_desc_index
        put_u32(&mut nsb, 140, 2); // nx_xp_desc_len
        put_u32(&mut nsb, 144, data_start as u32); // nx_xp_data_index
        put_u32(&mut nsb, 148, data_len as u32); // nx_xp_data_len
        if let Some(omap) = nx_omap_oid {
            put_u64(&mut nsb, 160, omap);
        }
        put_u64(&mut nsb, 1264, sb.flags & !NX_RESERVED_2);
        seal(&mut nsb);

        // Ordering is what makes this crash-safe: the new superblock is
        // written only after everything it points to is durable. Its two
        // copies (checkpoint area, then block 0) can share one flush: each
        // points only at durable data, and readers take the newest checkpoint
        // from the checkpoint area whatever block 0 says.
        let t_write = Instant::now();
        let mut all: Vec<(u64, Vec<u8>)> = writes.into_iter().chain(eph_writes).collect();
        all.sort_by_key(|w| w.0);
        // Neighbouring blocks go out as one write: each request is a round trip.
        let mut i = 0;
        while i < all.len() {
            let start = all[i].0;
            if start == 0 || start >= sb.block_count {
                return Err(corrupt(format!("refusing to write outside the container: block {start:#x}")));
            }
            let mut buf = std::mem::take(&mut all[i].1);
            i += 1;
            while i < all.len() && all[i].0 == start + buf.len() as u64 / bs && buf.len() < 8 << 20 {
                if all[i].0 >= sb.block_count {
                    return Err(corrupt(format!("refusing to write outside the container: block {:#x}", all[i].0)));
                }
                buf.extend_from_slice(&all[i].1);
                i += 1;
            }
            self.dev.write_at(start * bs, &buf)?;
        }
        self.dev.write_at(cpm_paddr * bs, &cpm)?;
        let t_flush = Instant::now();
        self.dev.flush()?;
        let flush1 = t_flush.elapsed();
        self.dev.write_at((desc_base + sb_pos) * bs, &nsb)?;
        self.dev.write_at(0, &nsb)?;
        self.dev.flush()?;
        let t_reload = Instant::now();

        self.reload()?;
        if self.c.xid() != xid {
            return Err(corrupt(format!("after committing xid {xid} the newest checkpoint is {}", self.c.xid())));
        }
        let total = t_start.elapsed();
        if total.as_secs_f64() > 1.0 || tracing() {
            log(&format!(
                "winapfs: commit {xid} took {:.2} s ({bitmap_reads} bitmaps read): prepare {:.2}, write {:.2}, flushes {:.2} (first {:.2}), reload {:.2}",
                total.as_secs_f64(),
                (t_write - t_start).as_secs_f64(),
                (t_flush - t_write).as_secs_f64(),
                (t_reload - t_flush).as_secs_f64(),
                flush1.as_secs_f64(),
                t_reload.elapsed().as_secs_f64()
            ));
        }
        Ok(xid)
    }
}

/// One volume open for changes, as a mounted file system uses it: changes
/// to names and folders commit at once; file data collects in an open
/// transaction until [`Session::flush`] (or the next change that commits).
pub struct Session {
    w: Writer,
    vol: usize,
    open: Option<(Txn, VolState)>,
    /// Keep every change in one transaction until an explicit flush.
    batch: bool,
    /// Why changes that had already been accepted were thrown away, until
    /// [`Session::take_lost`] reports it.
    lost: Option<String>,
}

impl Session {
    pub fn open(path: &Path, vol: usize) -> Result<Self> {
        let w = Writer::open(path)?;
        w.c.volume(vol)?;
        Ok(Self { w, vol, open: None, batch: false, lost: None })
    }

    /// A session on a real disk; see [`Writer::open_real_disk`].
    pub fn open_real_disk(path: &Path, base: u64, expected_uuid: [u8; 16], vol: usize) -> Result<Self> {
        let w = Writer::open_real_disk(path, base, expected_uuid)?;
        w.c.volume(vol)?;
        Ok(Self { w, vol, open: None, batch: false, lost: None })
    }

    /// A session on a real disk whose writes stay in the returned overlay.
    pub fn rehearsal(base: FileDevice, vol: usize) -> Result<(Self, Arc<Overlay>)> {
        let (w, ov) = Writer::rehearsal(base)?;
        w.c.volume(vol)?;
        Ok((Self { w, vol, open: None, batch: false, lost: None }, ov))
    }

    /// Why changes that earlier calls had accepted were thrown away (a
    /// failed commit, or a failure that left a later change half made), if
    /// that happened since the last call. The volume is back at its last
    /// commit, so whoever was told those changes succeeded must be told now.
    pub fn take_lost(&mut self) -> Option<String> {
        self.lost.take()
    }

    /// The committed state, as a reader sees it. Files being written read
    /// through [`Session::read_pending`] instead.
    pub fn volume(&self) -> Result<Volume<'_>> {
        self.w.c.volume(self.vol)
    }

    pub fn container(&self) -> &Container {
        &self.w.c
    }

    /// Whether changes are waiting for [`Session::flush`].
    pub fn dirty(&self) -> bool {
        self.open.as_ref().is_some_and(|(_, st)| st.changed())
    }

    /// The size of file `id` if it is being written.
    pub fn pending_size(&self, id: u64) -> Option<u64> {
        self.open.as_ref().and_then(|(_, st)| st.pending_size(id))
    }

    /// Reads file `id` if it is being written; `None` otherwise.
    pub fn read_pending(&mut self, id: u64, off: u64, buf: &mut [u8]) -> Result<Option<usize>> {
        if self.pending_size(id).is_none() {
            return Ok(None);
        }
        let v = self.w.c.volume(self.vol)?;
        let (_, st) = self.open.as_mut().unwrap();
        VolEdit { v: &v, st }.read_pending(id, off, buf)
    }

    fn begin_if_needed(&mut self) -> Result<()> {
        if self.open.is_none() {
            let t = self.w.begin()?;
            let st = VolState::open(&self.w.c.volume(self.vol)?)?;
            self.open = Some((t, st));
        }
        Ok(())
    }

    /// Refuses a change that would need `bytes` of new data blocks when that
    /// would eat into [`METADATA_RESERVE`]: refused here, before anything
    /// changes, a full drive fails this one change instead of the batch.
    fn ensure_space(&mut self, bytes: u64) -> Result<()> {
        self.begin_if_needed()?;
        let bs = self.w.c.block_size() as u64;
        let (t, _) = self.open.as_ref().unwrap();
        if t.space.free_blocks() < bytes.div_ceil(bs) + METADATA_RESERVE / bs {
            return Err(Error::NoSpace("the drive is full".into()));
        }
        Ok(())
    }

    /// Runs `f` on the open transaction, starting one if needed. A failure
    /// that may have left changes half made discards the transaction.
    fn edit<R>(&mut self, f: impl FnOnce(&mut VolEdit, &mut Txn, &dyn WriteDevice) -> Result<R>) -> Result<R> {
        self.begin_if_needed()?;
        let v = self.w.c.volume(self.vol)?;
        let (t, st) = self.open.as_mut().unwrap();
        let had_changes = st.changed();
        let r = f(&mut VolEdit { v: &v, st }, t, &*self.w.dev);
        drop(v);
        if let Err(e) = &r {
            // These are found before anything changes.
            if !matches!(e, Error::NotFound(_) | Error::Exists(_) | Error::InvalidName(_) | Error::NotEmpty(_)) {
                if had_changes {
                    self.lost = Some(e.to_string());
                }
                self.open = None;
            }
        }
        r
    }

    /// Commits the open transaction, if there is one with changes. On
    /// failure the changes are gone; see [`Session::take_lost`].
    pub fn flush(&mut self) -> Result<()> {
        let Some((t, st)) = self.open.take() else { return Ok(()) };
        if !st.changed() {
            return Ok(());
        }
        let r = self.commit_changes(t, st);
        if let Err(e) = &r {
            self.lost = Some(e.to_string());
        }
        r
    }

    fn commit_changes(&mut self, mut t: Txn, st: VolState) -> Result<()> {
        let v = self.w.c.volume(self.vol)?;
        self.w.finish_volume(&mut t, &v, st)?;
        drop(v);
        self.w.commit(t)?;
        Ok(())
    }

    /// Whether the open transaction has queued so many frees that its
    /// checkpoint could outgrow the free queues; then it commits early.
    fn batch_full(&self) -> bool {
        self.open.as_ref().is_some_and(|(t, _)| t.space.queued_frees() > t.space.queued_free_limit())
    }

    fn edit_and_flush<R>(&mut self, f: impl FnOnce(&mut VolEdit, &mut Txn, &dyn WriteDevice) -> Result<R>) -> Result<R> {
        let r = self.edit(f)?;
        if !self.batch || self.batch_full() {
            self.flush()?;
        }
        Ok(r)
    }

    /// Batch mode: every change stays in one transaction until
    /// [`Session::flush`]. Until a later transaction runs, nothing the
    /// previous checkpoint uses has been overwritten.
    pub fn set_batch(&mut self, batch: bool) {
        self.batch = batch;
    }

    /// The entries of folder `dir`, seeing changes not yet committed:
    /// (name, inode), with the sizes of files being written.
    pub fn list(&mut self, dir: u64) -> Result<Vec<(String, Inode)>> {
        if self.open.is_none() {
            let v = self.volume()?;
            return v.read_dir(dir)?.into_iter().map(|e| Ok((e.name, v.inode(e.file_id)?))).collect();
        }
        let mut entries = self.edit(|e, _, _| e.list(dir))?;
        for (_, inode) in &mut entries {
            if let Some(size) = self.pending_size(inode.id) {
                let alloced_size = inode.dstream.map_or(0, |d| d.alloced_size);
                inode.dstream = Some(crate::fs::Dstream { size, alloced_size });
            }
        }
        Ok(entries)
    }

    /// Whether a transaction is open (changes not yet committed, or none yet).
    pub fn in_transaction(&self) -> bool {
        self.open.is_some()
    }

    /// The inode number at `path`, seeing changes not yet committed.
    pub fn lookup_path(&mut self, path: &str) -> Result<u64> {
        if self.open.is_none() {
            return Ok(self.volume()?.lookup_path(path)?.id);
        }
        let comps: Vec<String> = path.split('/').filter(|c| !c.is_empty()).map(str::to_string).collect();
        self.edit(|e, _, _| {
            let mut id = vol::ROOT_DIR_INO;
            for c in &comps {
                id = e.lookup(id, c)?.ok_or_else(|| Error::NotFound(format!("\"{c}\" in {path}")))?;
            }
            Ok(id)
        })
    }

    /// Creates an empty file or folder; returns its inode number.
    pub fn create(&mut self, dir: u64, name: &str, kind: NewKind) -> Result<u64> {
        self.ensure_space(0)?;
        let now = now_ns();
        self.edit_and_flush(|e, _, _| e.create(dir, name, kind, now))
    }

    /// Creates several entries in one transaction.
    pub fn create_many(&mut self, dir: u64, names: &[&str], kind: NewKind) -> Result<Vec<u64>> {
        self.ensure_space(0)?;
        let now = now_ns();
        self.edit_and_flush(|e, _, _| names.iter().map(|n| e.create(dir, n, kind, now)).collect())
    }

    /// Deletes a file or an empty folder.
    pub fn remove(&mut self, dir: u64, name: &str) -> Result<()> {
        let now = now_ns();
        self.edit_and_flush(|e, t, _| e.unlink(t, dir, name, now))
    }

    /// Moves or renames an entry; see [`VolEdit::rename`].
    pub fn rename(&mut self, sdir: u64, sname: &str, ddir: u64, dname: &str, replace: bool) -> Result<()> {
        let now = now_ns();
        self.edit_and_flush(|e, t, _| e.rename(t, sdir, sname, ddir, dname, replace, now))
    }

    pub fn set_times(&mut self, id: u64, times: Times) -> Result<()> {
        self.edit_and_flush(|e, _, _| e.set_times(id, times))
    }

    pub fn set_hidden(&mut self, id: u64, hidden: bool) -> Result<()> {
        self.edit_and_flush(|e, _, _| e.set_hidden(id, hidden))
    }

    /// Writes into file `id` (kept in the open transaction).
    pub fn write(&mut self, id: u64, off: u64, data: &[u8]) -> Result<()> {
        self.ensure_space(data.len() as u64)?;
        self.edit(|e, t, dev| e.write(t, dev, id, off, data))?;
        if self.batch_full() {
            self.flush()?;
        }
        Ok(())
    }

    /// Changes the size of file `id` (kept in the open transaction).
    pub fn set_size(&mut self, id: u64, size: u64) -> Result<()> {
        self.edit(|e, t, dev| e.set_size(t, dev, id, size))?;
        if self.batch_full() {
            self.flush()?;
        }
        Ok(())
    }

    /// The inode as it stands, including changes not yet committed.
    pub fn inode(&mut self, id: u64) -> Result<Inode> {
        if self.open.is_none() {
            return self.volume()?.inode(id);
        }
        let mut inode = self.edit(|e, _, _| e.inode(id))?;
        if let Some(size) = self.pending_size(id) {
            let alloced_size = inode.dstream.map_or(0, |d| d.alloced_size);
            inode.dstream = Some(crate::fs::Dstream { size, alloced_size });
        }
        Ok(inode)
    }
}

/// Writes saved blocks back to the container at byte `base` of `path` (a
/// disk or an image), for undoing a transaction: `blocks` are (block
/// address, contents), written in order, then flushed. Refused unless the
/// container's UUID is `expected_uuid`.
pub fn restore_blocks(path: &Path, base: u64, expected_uuid: [u8; 16], blocks: &[(u64, Vec<u8>)]) -> Result<()> {
    let c = Container::open(Box::new(FileDevice::open(path)?.with_base(base)?))?;
    if c.sb.uuid != expected_uuid {
        return Err(unsupported(format!("container {} is not the one the undo file belongs to", crate::format_uuid(&c.sb.uuid))));
    }
    let bs = c.block_size() as u64;
    let dev = DiskFile::open_rw(path, base)?;
    for (paddr, data) in blocks {
        if data.len() as u64 % bs != 0 || paddr + data.len() as u64 / bs > c.sb.block_count {
            return Err(corrupt(format!("undo data for block {paddr:#x} does not fit the container")));
        }
        dev.write_at(paddr * bs, data)?;
    }
    dev.flush()
}

/// Measures raw write speed on the container at byte `base` of `path`:
/// reserves `mib` MiB of free space in a transaction that is never
/// committed, writes test data there in pieces of each size in `chunks`,
/// and returns (piece size, seconds, seconds to flush). Nothing in use is
/// touched and the container does not change.
pub fn bench_raw_writes(path: &Path, base: u64, expected_uuid: [u8; 16], mib: u64, chunks: &[usize]) -> Result<Vec<(usize, f64, f64)>> {
    let w = Writer::open_real_disk(path, base, expected_uuid)?;
    let mut t = w.begin()?;
    let bs = w.c.block_size() as u64;
    let want = mib * 1024 * 1024 / bs;
    let mut runs = Vec::new();
    let mut got = 0;
    while got < want {
        let (start, n) = t.space.alloc_run(&w.c, want - got)?;
        runs.push((start, n));
        got += n;
    }
    let pattern: Vec<u8> = (0..16 << 20).map(|i: u32| (i % 251) as u8).collect();
    let mut out = Vec::new();
    for &chunk in chunks {
        let start = Instant::now();
        for &(first, n) in &runs {
            let (mut off, end) = (first * bs, (first + n) * bs);
            while off < end {
                let len = (chunk as u64).min(end - off) as usize;
                w.dev.write_at(off, &pattern[..len])?;
                off += len as u64;
            }
        }
        let wrote = start.elapsed().as_secs_f64();
        let f = Instant::now();
        w.dev.flush()?;
        out.push((chunk, wrote, f.elapsed().as_secs_f64()));
    }
    drop(t); // never committed: the blocks stay free
    Ok(out)
}

/// What would stop this crate from writing to container `c`, found by
/// loading everything a transaction loads (nothing is written). Empty when
/// writing is possible.
pub fn check_writable(c: &Container) -> Vec<String> {
    let mut problems = Vec::new();
    let sb = &c.sb;
    if sb.xp_desc_blocks & 0x8000_0000 != 0 || sb.xp_data_blocks & 0x8000_0000 != 0 {
        problems.push("checkpoint areas are not contiguous".into());
    }
    let per_block = (c.block_size() as usize - 40) / 40;
    if c.checkpoint_map.len() > per_block {
        problems.push(format!("checkpoint map has {} entries (one block holds {per_block})", c.checkpoint_map.len()));
    }
    match Space::load(c, sb.next_xid) {
        Ok(mut s) => {
            if let Err(e) = s.release_queued(c) {
                problems.push(format!("space manager, releasing freed blocks: {e}"));
            }
        }
        Err(e) => problems.push(format!("space manager: {e}")),
    }
    for i in 0..sb.fs_oids.len() {
        match c.volume(i) {
            Ok(v) => {
                if let Err(e) = VolState::open(&v) {
                    problems.push(format!("volume {i} \"{}\": {e}", v.sb.name));
                }
                if let Err(e) = Tree::open(&PhysReader(c), v.sb.extentref_tree_oid, fs::key_cmp) {
                    problems.push(format!("volume {i} \"{}\" extent-reference tree: {e}", v.sb.name));
                }
            }
            Err(e) => problems.push(format!("volume {i}: {e}")),
        }
    }
    problems
}

/// Records this program in a volume superblock's modification history
/// (`apfs_modified_by`, newest first) and its last-modified time.
fn stamp_modified_by(apsb: &mut [u8], xid: u64) {
    const HISTORY: usize = 320;
    const ENTRY: usize = 48;
    let now = now_ns();
    let mut entry = [0u8; ENTRY];
    entry[..MODIFIED_BY.len()].copy_from_slice(MODIFIED_BY.as_bytes());
    entry[32..40].copy_from_slice(&now.to_le_bytes());
    entry[40..48].copy_from_slice(&xid.to_le_bytes());
    // Same program as the newest entry: update it; otherwise push a new one.
    if apsb[HISTORY..HISTORY + 32] != entry[..32] {
        apsb.copy_within(HISTORY..HISTORY + 7 * ENTRY, HISTORY + ENTRY);
    }
    apsb[HISTORY..HISTORY + ENTRY].copy_from_slice(&entry);
    put_u64(apsb, 256, now); // apfs_last_mod_time
}
