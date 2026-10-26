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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::btree::Node;
use crate::btree_write::Entry;
use crate::bytes::u64_at;
use crate::checksum;
use crate::dev::{FileDevice, ImageFile, WriteDevice};
use crate::error::{corrupt, unsupported, Error, Result};
use crate::fs::Volume;
use crate::gpt;
use crate::nx::{CheckpointMapping, Container};
use crate::obj::{OBJ_PHYSICAL, OBJECT_TYPE_CHECKPOINT_MAP};
use space::Space;
use tree::{omap_cmp, omap_drop, omap_put, PhysReader, Reader, Tree};

const CHECKPOINT_MAP_LAST: u32 = 0x1;
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

fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// An APFS container in an image file, open for writing.
pub struct Writer {
    c: Container,
    dev: ImageFile,
    path: std::path::PathBuf,
    base: u64,
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

/// What [`Writer::create`] makes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NewKind {
    File,
    Folder,
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
        let c = Container::open(Box::new(FileDevice::open(path)?.with_base(base)?))?;
        Ok(Self { c, dev, path: path.to_path_buf(), base })
    }

    /// The committed state, as a reader sees it.
    pub fn container(&self) -> &Container {
        &self.c
    }

    fn reload(&mut self) -> Result<()> {
        self.c = Container::open(Box::new(FileDevice::open(&self.path)?.with_base(self.base)?))?;
        Ok(())
    }

    /// Starts a transaction. Blocks that earlier transactions freed become
    /// reusable now.
    pub fn begin(&self) -> Result<Txn> {
        let xid = self.c.sb.next_xid;
        let mut space = Space::load(&self.c, xid)?;
        space.release_queued(&self.c)?;
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

    /// Creates an empty file or folder `name` in folder `dir_id` of volume
    /// `vol`; returns the new inode number and the transaction id.
    pub fn create(&mut self, vol: usize, dir_id: u64, name: &str, kind: NewKind) -> Result<(u64, u64)> {
        let (ids, xid) = self.create_many(vol, dir_id, &[name], kind)?;
        Ok((ids[0], xid))
    }

    /// Creates several entries in one folder in a single transaction.
    pub fn create_many(&mut self, vol: usize, dir_id: u64, names: &[&str], kind: NewKind) -> Result<(Vec<u64>, u64)> {
        let mut t = self.begin()?;
        let now = now_ns();
        let v = self.c.volume(vol)?;
        let mut e = VolEdit::open(&v)?;
        let ids = names.iter().map(|name| e.create(dir_id, name, kind, now)).collect::<Result<Vec<_>>>()?;
        self.finish_volume(&mut t, e)?;
        drop(v);
        Ok((ids, self.commit(t)?))
    }

    /// Writes a volume's changes: its file-system tree, its object map and a
    /// new copy of its superblock.
    fn finish_volume(&self, t: &mut Txn, e: VolEdit) -> Result<()> {
        if t.nx_omap_oid.is_some() {
            return Err(unsupported("changing two volumes in one transaction"));
        }
        let VolEdit { v, tree, mut apsb } = e;
        let (mut alloced, mut freed) = (0, 0);
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
        // Blocks the volume uses (its trees and object map), and running totals.
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
        let Txn { xid, space, mut writes, nx_omap_oid, next_oid } = t;
        let ephemeral: HashMap<u64, Vec<u8>> = space.finish(&mut writes)?.into_iter().collect();

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
        for m in &self.c.checkpoint_map {
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
        // written only after everything it points to is durable.
        for (paddr, block) in writes.iter().chain(&eph_writes) {
            if *paddr == 0 || *paddr >= sb.block_count {
                return Err(corrupt(format!("refusing to write outside the container: block {paddr:#x}")));
            }
            self.dev.write_at(paddr * bs, block)?;
        }
        self.dev.write_at(cpm_paddr * bs, &cpm)?;
        self.dev.flush()?;
        self.dev.write_at((desc_base + sb_pos) * bs, &nsb)?;
        self.dev.flush()?;
        self.dev.write_at(0, &nsb)?;
        self.dev.flush()?;

        self.reload()?;
        if self.c.xid() != xid {
            return Err(corrupt(format!("after committing xid {xid} the newest checkpoint is {}", self.c.xid())));
        }
        Ok(xid)
    }
}

/// Reads a volume's file-system tree nodes through its object map.
struct VolReader<'a, 'c>(&'a Volume<'c>);

impl Reader for VolReader<'_, '_> {
    fn read(&self, oid: u64) -> Result<(Arc<Node>, u64)> {
        self.0.fs_node(oid)
    }
}

/// A volume being changed in a transaction: its file-system tree, edited
/// copy-on-write, and a copy of its superblock.
struct VolEdit<'a, 'c> {
    v: &'a Volume<'c>,
    tree: Tree,
    apsb: Vec<u8>,
}

impl<'a, 'c> VolEdit<'a, 'c> {
    fn open(v: &'a Volume<'c>) -> Result<Self> {
        if v.sb.num_snapshots > 0 {
            return Err(unsupported("writing to volumes that have snapshots"));
        }
        let tree = Tree::open(&VolReader(v), v.sb.root_tree_oid, fs::key_cmp)?;
        Ok(Self { v, tree, apsb: v.sb.raw.clone() })
    }

    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.tree.get(&VolReader(self.v), key)?.flatten())
    }

    fn put(&mut self, key: Vec<u8>, val: Vec<u8>) -> Result<()> {
        self.tree.upsert(&VolReader(self.v), key, Some(val))
    }

    fn range(&mut self, lo: &[u8], hi: &[u8]) -> Result<Vec<Entry>> {
        self.tree.range(&VolReader(self.v), lo, hi)
    }

    /// Adds `delta` to a superblock counter.
    fn bump(&mut self, off: usize, delta: i64) -> Result<()> {
        let v = u64_at(&self.apsb, off)?.wrapping_add_signed(delta);
        put_u64(&mut self.apsb, off, v);
        Ok(())
    }

    /// A new inode number (`apfs_next_obj_id`).
    fn take_id(&mut self) -> Result<u64> {
        let id = u64_at(&self.apsb, 176)?;
        put_u64(&mut self.apsb, 176, id + 1);
        Ok(id)
    }

    fn create(&mut self, dir_id: u64, name: &str, kind: NewKind, now: u64) -> Result<u64> {
        if name.is_empty() || name.len() > 255 || name.contains(['/', '\0']) || name == "." || name == ".." {
            return Err(Error::InvalidName(format!("{name:?}")));
        }
        let ci = self.v.sb.is_case_insensitive();
        let parent_key = fs::inode_key(dir_id);
        let mut parent = self.get(&parent_key)?.ok_or_else(|| Error::NotFound(format!("folder {dir_id}")))?;
        if !fs::is_dir(&parent) {
            return Err(Error::NotFound(format!("folder {dir_id} (it is a file)")));
        }
        let (lo, hi) = fs::drec_hash_range(dir_id, name, ci);
        let want = self.v.name_key(name);
        if self.range(&lo, &hi)?.iter().any(|(k, _)| self.v.name_key(&fs::drec_name(k)) == want) {
            return Err(Error::Exists(format!("{name:?}")));
        }

        let id = self.take_id()?;
        let (mode, dt, counter) = match kind {
            NewKind::File => (fs::S_IFREG, fs::DT_REG, 184),  // apfs_num_files
            NewKind::Folder => (fs::S_IFDIR, fs::DT_DIR, 192), // apfs_num_directories
        };
        // The folder: one more child, and its times move.
        let children = i32::from_le_bytes(parent[56..60].try_into().unwrap());
        parent[56..60].copy_from_slice(&(children + 1).to_le_bytes());
        put_u64(&mut parent, 24, now); // mod_time
        put_u64(&mut parent, 32, now); // change_time
        self.put(parent_key, parent)?;
        self.put(fs::inode_key(id), fs::new_inode(dir_id, id, name, now, mode))?;
        self.put(fs::drec_key(dir_id, name, ci), fs::drec_val(id, now, dt))?;
        self.bump(counter, 1)?;
        Ok(id)
    }
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
