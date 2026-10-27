//! Changing one volume within a transaction: its file-system tree and
//! extent-reference tree, edited copy-on-write, and the data of files being
//! written.
//!
//! A file being written keeps its block map in memory ([`FileState`]): new
//! data goes straight to newly allocated blocks (free in every checkpoint
//! that can still be mounted, so writing them early is safe), and the
//! file's extent records are rewritten from that map when the transaction
//! commits. Blocks the committed state used are released at once through
//! the extent-reference tree; they stay allocated until the next
//! transaction, so the previous checkpoint stays intact.

use std::collections::HashMap;
use std::sync::Arc;

use super::fs;
use super::tree::{PhysReader, Reader, Tree};
use super::{put_u64, Txn};
use crate::btree::Node;
use crate::btree_write::Entry;
use crate::bytes::{u16_at, u32_at, u64_at};
use crate::dev::WriteDevice;
use crate::error::{corrupt, unsupported, Error, Result};
use crate::fs::{Inode, Volume};

/// What [`VolEdit::create`] makes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NewKind {
    File,
    Folder,
}

/// Times to set on an inode, in nanoseconds since 1970; `None` keeps one.
#[derive(Clone, Copy, Default, Debug)]
pub struct Times {
    pub create: Option<u64>,
    pub modify: Option<u64>,
    pub change: Option<u64>,
    pub access: Option<u64>,
}

const XATTR_DATA_STREAM: u16 = 0x1;
const APFS_TYPE_SIBLING_LINK: u64 = 5;
pub(super) const ROOT_DIR_INO: u64 = 2;
/// Most file data queued before it is written out (8 MiB).
const PENDING_IO_MAX: usize = 8 << 20;

/// Reads a volume's file-system tree nodes through its object map.
pub(super) struct VolReader<'a, 'c>(pub &'a Volume<'c>);

impl Reader for VolReader<'_, '_> {
    fn read(&self, oid: u64) -> Result<(Arc<Node>, u64)> {
        self.0.fs_node(oid)
    }
}

/// `blocks` blocks of a file, from logical block `lblock`, stored from
/// block `phys`. Fresh runs were allocated in the open transaction and may
/// be rewritten in place; the others belong to the committed state.
#[derive(Clone, Copy, Debug)]
struct Run {
    lblock: u64,
    phys: u64,
    blocks: u64,
    fresh: bool,
}

/// A file whose data is changing in the open transaction.
struct FileState {
    stream: u64,
    size: u64,
    runs: Vec<Run>,
    written: u64,
    /// Data changed since the extent records were last rewritten.
    dirty: bool,
}

/// One volume's changes in the open transaction.
pub(super) struct VolState {
    pub tree: Tree,
    pub extref: Option<Tree>,
    pub apsb: Vec<u8>,
    /// File data blocks allocated and freed.
    pub data_alloced: u64,
    pub data_freed: u64,
    files: HashMap<u64, FileState>,
    /// File data waiting to be written: (first block, bytes), contiguous.
    pending_io: Option<(u64, Vec<u8>)>,
}

impl VolState {
    pub fn open(v: &Volume) -> Result<Self> {
        if v.sb.num_snapshots > 0 {
            return Err(unsupported("writing to volumes that have snapshots"));
        }
        let tree = Tree::open(&VolReader(v), v.sb.root_tree_oid, fs::key_cmp)?;
        Ok(Self { tree, extref: None, apsb: v.sb.raw.clone(), data_alloced: 0, data_freed: 0, files: HashMap::new(), pending_io: None })
    }

    pub fn changed(&self) -> bool {
        self.tree.changed() || self.extref.as_ref().is_some_and(Tree::changed) || !self.files.is_empty()
    }

    /// Size of a file being written, if it is.
    pub fn pending_size(&self, id: u64) -> Option<u64> {
        self.files.get(&id).map(|f| f.size)
    }
}

/// A volume's committed state plus its open changes.
pub(super) struct VolEdit<'a, 'c> {
    pub v: &'a Volume<'c>,
    pub st: &'a mut VolState,
}

impl<'a, 'c> VolEdit<'a, 'c> {
    fn bs(&self) -> u64 {
        self.v.container().block_size() as u64
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.st.tree.get(&VolReader(self.v), key)?.flatten())
    }

    pub fn put(&mut self, key: Vec<u8>, val: Vec<u8>) -> Result<()> {
        self.st.tree.upsert(&VolReader(self.v), key, Some(val))
    }

    fn remove(&mut self, key: &[u8]) -> Result<bool> {
        self.st.tree.remove(&VolReader(self.v), key)
    }

    fn range(&mut self, lo: &[u8], hi: &[u8]) -> Result<Vec<Entry>> {
        self.st.tree.range(&VolReader(self.v), lo, hi)
    }

    fn extref(&mut self) -> Result<&mut Tree> {
        if self.st.extref.is_none() {
            let rd = PhysReader(self.v.container());
            self.st.extref = Some(Tree::open(&rd, self.v.sb.extentref_tree_oid, fs::key_cmp)?);
        }
        Ok(self.st.extref.as_mut().unwrap())
    }

    /// Adds `delta` to a superblock counter.
    fn bump(&mut self, off: usize, delta: i64) -> Result<()> {
        let v = u64_at(&self.st.apsb, off)?.wrapping_add_signed(delta);
        put_u64(&mut self.st.apsb, off, v);
        Ok(())
    }

    /// A new inode number (`apfs_next_obj_id`).
    fn take_id(&mut self) -> Result<u64> {
        let id = u64_at(&self.st.apsb, 176)?;
        put_u64(&mut self.st.apsb, 176, id + 1);
        Ok(id)
    }

    fn inode_raw(&mut self, id: u64) -> Result<Vec<u8>> {
        self.get(&fs::inode_key(id))?.ok_or_else(|| Error::NotFound(format!("inode {id}")))
    }

    /// The inode as it stands in the open transaction.
    pub fn inode(&mut self, id: u64) -> Result<Inode> {
        Inode::parse(id, &self.inode_raw(id)?)
    }

    fn folder(&mut self, id: u64) -> Result<Vec<u8>> {
        let raw = self.get(&fs::inode_key(id))?.ok_or_else(|| Error::NotFound(format!("folder {id}")))?;
        if !fs::is_dir(&raw) {
            return Err(Error::NotFound(format!("folder {id} (it is a file)")));
        }
        Ok(raw)
    }

    /// Adjusts a folder's child count and stamps its modification times.
    fn touch_folder(&mut self, dir: u64, delta: i32, now: u64) -> Result<()> {
        let mut raw = self.folder(dir)?;
        let children = i32::from_le_bytes(raw[56..60].try_into().unwrap());
        raw[56..60].copy_from_slice(&(children + delta).to_le_bytes());
        put_u64(&mut raw, 24, now); // mod_time
        put_u64(&mut raw, 32, now); // change_time
        self.put(fs::inode_key(dir), raw)
    }

    /// The directory entry for `name` in `dir`, matched as the volume
    /// compares names: (key, value).
    /// The entries of folder `dir` as they stand: (name, inode).
    pub fn list(&mut self, dir: u64) -> Result<Vec<(String, Inode)>> {
        let mut out = Vec::new();
        let lo = fs::key_bound_type(dir, fs::APFS_TYPE_DIR_REC);
        let hi = fs::key_bound_type(dir, fs::APFS_TYPE_DIR_REC + 1);
        for (k, v) in self.range(&lo, &hi)? {
            let Some(v) = v else { continue };
            let id = u64_at(&v, 0)?;
            match self.inode(id) {
                Ok(inode) => out.push((fs::drec_name(&k), inode)),
                Err(Error::NotFound(_)) => {} // left out rather than failing the folder
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// The inode number of `name` in folder `dir`, if it is there.
    pub fn lookup(&mut self, dir: u64, name: &str) -> Result<Option<u64>> {
        match self.find_entry(dir, name)? {
            Some((_, Some(v))) => Ok(Some(u64_at(&v, 0)?)),
            Some((_, None)) => Err(corrupt("directory entry without a value")),
            None => Ok(None),
        }
    }

    fn find_entry(&mut self, dir: u64, name: &str) -> Result<Option<Entry>> {
        let (lo, hi) = fs::drec_hash_range(dir, name, self.v.sb.is_case_insensitive());
        let want = self.v.name_key(name);
        Ok(self.range(&lo, &hi)?.into_iter().find(|(k, _)| self.v.name_key(&fs::drec_name(k)) == want))
    }

    pub fn create(&mut self, dir: u64, name: &str, kind: NewKind, now: u64) -> Result<u64> {
        check_name(name)?;
        self.folder(dir)?;
        if self.find_entry(dir, name)?.is_some() {
            return Err(Error::Exists(format!("{name:?}")));
        }
        let id = self.take_id()?;
        let (mode, dt, counter) = match kind {
            NewKind::File => (fs::S_IFREG, fs::DT_REG, 184),  // apfs_num_files
            NewKind::Folder => (fs::S_IFDIR, fs::DT_DIR, 192), // apfs_num_directories
        };
        self.touch_folder(dir, 1, now)?;
        self.put(fs::inode_key(id), fs::new_inode(dir, id, name, now, mode))?;
        let ci = self.v.sb.is_case_insensitive();
        self.put(fs::drec_key(dir, name, ci), fs::drec_val(id, now, dt))?;
        self.bump(counter, 1)?;
        Ok(id)
    }

    /// Removes the file or empty folder `name` from `dir`.
    pub fn unlink(&mut self, t: &mut Txn, dir: u64, name: &str, now: u64) -> Result<()> {
        let (key, val) = self.find_entry(dir, name)?.ok_or_else(|| Error::NotFound(format!("{name:?}")))?;
        let id = u64_at(val.as_deref().ok_or_else(|| corrupt("directory entry without a value"))?, 0)?;
        let raw = self.inode_raw(id)?;
        let children_or_links = i32::from_le_bytes(raw[56..60].try_into().unwrap());
        let is_dir = fs::is_dir(&raw);
        if is_dir && children_or_links != 0 {
            return Err(Error::NotEmpty(format!("{name:?}")));
        }
        if !is_dir && children_or_links != 1 {
            return Err(unsupported("deleting files with several hard links"));
        }
        let stream = u64_at(&raw, 8)?;
        // Everything keyed by the inode (and its data stream) goes.
        let mut records = self.range(&fs::key_bound(id), &fs::key_bound(id + 1))?;
        if stream != id {
            records.extend(self.range(&fs::key_bound(stream), &fs::key_bound(stream + 1))?);
        }
        if records.iter().any(|(k, _)| fs::key_type(k) == APFS_TYPE_SIBLING_LINK) {
            return Err(unsupported("deleting files with hard links"));
        }

        self.remove(&key)?;
        self.touch_folder(dir, -1, now)?;
        self.drop_file_data(t, id, stream)?;
        for (k, v) in records {
            if fs::key_type(&k) == fs::APFS_TYPE_XATTR {
                let v = v.unwrap_or_default();
                if u16_at(&v, 0)? & XATTR_DATA_STREAM != 0 {
                    let xstream = u64_at(&v, 4)?;
                    self.drop_extents(t, xstream)?;
                    self.remove(&fs::dstream_id_key(xstream))?;
                }
            }
            // File extents were removed with the data.
            self.remove(&k)?;
        }
        self.bump(if is_dir { 192 } else { 184 }, -1)
    }

    /// Moves `sname` in `sdir` to `dname` in `ddir`. An existing target is
    /// replaced only if `replace` is set and it is a file, or an empty
    /// folder when moving a folder.
    pub fn rename(&mut self, t: &mut Txn, sdir: u64, sname: &str, ddir: u64, dname: &str, replace: bool, now: u64) -> Result<()> {
        check_name(dname)?;
        let (skey, sval) = self.find_entry(sdir, sname)?.ok_or_else(|| Error::NotFound(format!("{sname:?}")))?;
        let sval = sval.ok_or_else(|| corrupt("directory entry without a value"))?;
        let id = u64_at(&sval, 0)?;
        self.folder(ddir)?;
        let mut raw = self.inode_raw(id)?;
        if fs::is_dir(&raw) {
            // A folder cannot move into itself or below itself.
            let mut up = ddir;
            while up > ROOT_DIR_INO {
                if up == id {
                    return Err(Error::InvalidName(format!("{dname:?}: a folder cannot move into itself")));
                }
                up = u64_at(&self.inode_raw(up)?, 0)?;
            }
        }
        if let Some((_, dval)) = self.find_entry(ddir, dname)? {
            let did = u64_at(dval.as_deref().unwrap_or_default(), 0)?;
            if did != id {
                if !replace {
                    return Err(Error::Exists(format!("{dname:?}")));
                }
                let draw = self.inode_raw(did)?;
                if fs::is_dir(&draw) != fs::is_dir(&raw) {
                    return Err(Error::Exists(format!("{dname:?} (a {} is in the way)", if fs::is_dir(&draw) { "folder" } else { "file" })));
                }
                self.unlink(t, ddir, dname, now)?;
            }
        }
        self.remove(&skey)?;
        let ci = self.v.sb.is_case_insensitive();
        self.put(fs::drec_key(ddir, dname, ci), sval)?;
        if sdir != ddir {
            self.touch_folder(sdir, -1, now)?;
            self.touch_folder(ddir, 1, now)?;
            put_u64(&mut raw, 0, ddir); // parent_id
        } else {
            self.touch_folder(sdir, 0, now)?;
        }
        let (mut fixed, mut fields) = fs::split_inode(&raw)?;
        let mut name_z = dname.as_bytes().to_vec();
        name_z.push(0);
        fs::set_xfield(&mut fields, fs::INO_EXT_TYPE_NAME, fs::XF_DO_NOT_COPY, name_z);
        put_u64(&mut fixed, 32, now); // change_time
        self.put(fs::inode_key(id), fs::join_inode(&fixed, &fields))
    }

    pub fn set_times(&mut self, id: u64, times: Times) -> Result<()> {
        self.flush_file(id)?;
        let mut raw = self.inode_raw(id)?;
        for (off, t) in [(16, times.create), (24, times.modify), (32, times.change), (40, times.access)] {
            if let Some(t) = t {
                put_u64(&mut raw, off, t);
            }
        }
        self.put(fs::inode_key(id), raw)
    }

    /// Sets or clears the hidden flag.
    pub fn set_hidden(&mut self, id: u64, hidden: bool) -> Result<()> {
        const UF_HIDDEN: u32 = 0x8000;
        let mut raw = self.inode_raw(id)?;
        let flags = u32_at(&raw, 68)?;
        let new = if hidden { flags | UF_HIDDEN } else { flags & !UF_HIDDEN };
        if new != flags {
            raw[68..72].copy_from_slice(&new.to_le_bytes());
            self.put(fs::inode_key(id), raw)?;
        }
        Ok(())
    }

    /// The block map of file `id`, loaded from its extent records on first use.
    fn file(&mut self, id: u64) -> Result<&mut FileState> {
        if !self.st.files.contains_key(&id) {
            let raw = self.inode_raw(id)?;
            if !fs::is_regular(&raw) {
                return Err(unsupported("writing data to something other than a regular file"));
            }
            if u32_at(&raw, 68)? & fs::UF_COMPRESSED != 0 {
                return Err(unsupported("changing compressed files"));
            }
            let stream = u64_at(&raw, 8)?;
            let (_, fields) = fs::split_inode(&raw)?;
            let (size, written) = match fields.iter().find(|f| f.0 == fs::INO_EXT_TYPE_DSTREAM) {
                Some(d) => (u64_at(&d.2, 0)?, u64_at(&d.2, 24)?),
                None => (0, 0),
            };
            let bs = self.bs();
            let (lo, hi) = fs::file_extent_range(stream);
            let mut runs = Vec::new();
            for (k, v) in self.range(&lo, &hi)? {
                let v = v.ok_or_else(|| corrupt("file extent without a value"))?;
                let (len, phys) = (u64_at(&v, 0)? & fs::FILE_EXTENT_LEN_MASK, u64_at(&v, 8)?);
                if u64_at(&v, 16)? != 0 {
                    return Err(unsupported("encrypted file data"));
                }
                if phys != 0 {
                    runs.push(Run { lblock: u64_at(&k, 8)? / bs, phys, blocks: len.div_ceil(bs), fresh: false });
                }
            }
            self.st.files.insert(id, FileState { stream, size, runs, written, dirty: false });
        }
        Ok(self.st.files.get_mut(&id).unwrap())
    }

    /// Queues file data for block `phys`: writes that continue the queued
    /// run join it (up to [`PENDING_IO_MAX`]), rewrites inside it update it,
    /// anything else writes the queue out first. Disks take few large writes
    /// far faster than many small ones.
    fn stage(&mut self, dev: &dyn WriteDevice, phys: u64, data: Vec<u8>) -> Result<()> {
        let bs = self.bs();
        let blocks = data.len() as u64 / bs;
        if let Some((start, buf)) = &mut self.st.pending_io {
            let end = *start + buf.len() as u64 / bs;
            if phys >= *start && phys + blocks <= end {
                let at = ((phys - *start) * bs) as usize;
                buf[at..at + data.len()].copy_from_slice(&data);
                return Ok(());
            }
            if phys == end && buf.len() + data.len() <= PENDING_IO_MAX {
                buf.extend_from_slice(&data);
                return Ok(());
            }
        }
        self.write_pending_io(dev)?;
        if data.len() >= PENDING_IO_MAX {
            return dev.write_at(phys * bs, &data);
        }
        self.st.pending_io = Some((phys, data));
        Ok(())
    }

    /// Writes out queued file data.
    pub fn write_pending_io(&mut self, dev: &dyn WriteDevice) -> Result<()> {
        if let Some((start, buf)) = self.st.pending_io.take() {
            dev.write_at(start * self.bs(), &buf)?;
        }
        Ok(())
    }

    /// Reads whole blocks from `phys`, seeing queued file data.
    fn read_fresh(&self, phys: u64, out: &mut [u8]) -> Result<()> {
        let bs = self.bs();
        self.v.container().r.read_at(phys * bs, out)?;
        if let Some((start, buf)) = &self.st.pending_io {
            let (a, b) = (phys * bs, phys * bs + out.len() as u64);
            let (qa, qb) = (start * bs, start * bs + buf.len() as u64);
            let (s, e) = (a.max(qa), b.min(qb));
            if s < e {
                out[(s - a) as usize..(e - a) as usize].copy_from_slice(&buf[(s - qa) as usize..(e - qa) as usize]);
            }
        }
        Ok(())
    }

    /// Reads a file being written; `None` if it is not.
    pub fn read_pending(&self, id: u64, off: u64, buf: &mut [u8]) -> Result<Option<usize>> {
        let Some(f) = self.st.files.get(&id) else { return Ok(None) };
        if off >= f.size {
            return Ok(Some(0));
        }
        let len = (buf.len() as u64).min(f.size - off) as usize;
        let bs = self.bs();
        let buf = &mut buf[..len];
        buf.fill(0);
        let end = off + len as u64;
        for r in &f.runs {
            let (rs, re) = (r.lblock * bs, (r.lblock + r.blocks) * bs);
            let (s, e) = (rs.max(off), re.min(end));
            if s < e {
                // Whole blocks only: raw disks refuse unaligned reads.
                let (first, last) = ((s - rs) / bs, (e - rs).div_ceil(bs));
                let mut tmp = vec![0u8; ((last - first) * bs) as usize];
                self.read_fresh(r.phys + first, &mut tmp)?;
                let skip = (s - rs - first * bs) as usize;
                buf[(s - off) as usize..(e - off) as usize].copy_from_slice(&tmp[skip..skip + (e - s) as usize]);
            }
        }
        Ok(Some(len))
    }

    /// Writes `data` at byte `off` of file `id`.
    pub fn write(&mut self, t: &mut Txn, dev: &dyn WriteDevice, id: u64, off: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let size = self.file(id)?.size;
        if off > size {
            self.zero_tail(t, dev, id)?;
        }
        self.write_range(t, dev, id, off, data)?;
        let f = self.file(id)?;
        f.size = f.size.max(off + data.len() as u64);
        f.written += data.len() as u64;
        f.dirty = true;
        Ok(())
    }

    /// Changes a file's size: blocks past the end are released; growing
    /// leaves a hole that reads as zeros.
    pub fn set_size(&mut self, t: &mut Txn, dev: &dyn WriteDevice, id: u64, size: u64) -> Result<()> {
        let bs = self.bs();
        let old = self.file(id)?.size;
        if size > old {
            self.zero_tail(t, dev, id)?;
        } else if size < old {
            let keep = size.div_ceil(bs);
            let f = self.file(id)?;
            let mut cut = Vec::new();
            let mut runs = Vec::new();
            for r in f.runs.drain(..) {
                if r.lblock + r.blocks <= keep {
                    runs.push(r);
                } else if r.lblock >= keep {
                    cut.push(r);
                } else {
                    let k = keep - r.lblock;
                    runs.push(Run { blocks: k, ..r });
                    cut.push(Run { lblock: keep, phys: r.phys + k, blocks: r.blocks - k, fresh: r.fresh });
                }
            }
            f.runs = runs;
            for r in cut {
                self.discard(t, r)?;
            }
        }
        let f = self.file(id)?;
        f.size = size;
        f.dirty = true;
        Ok(())
    }

    /// Zeroes the rest of the block holding the end of the file, so that
    /// growing the file exposes zeros.
    fn zero_tail(&mut self, t: &mut Txn, dev: &dyn WriteDevice, id: u64) -> Result<()> {
        let bs = self.bs();
        let size = self.file(id)?.size;
        if size % bs != 0 && self.find_run(id, size / bs)?.is_some() {
            let zeros = vec![0u8; (bs - size % bs) as usize];
            self.write_range(t, dev, id, size, &zeros)?;
        }
        Ok(())
    }

    fn find_run(&mut self, id: u64, lblock: u64) -> Result<Option<Run>> {
        Ok(self.file(id)?.runs.iter().find(|r| r.lblock <= lblock && lblock < r.lblock + r.blocks).copied())
    }

    /// Writes bytes into the block map: fresh blocks in place, others
    /// copy-on-write into newly allocated runs.
    fn write_range(&mut self, t: &mut Txn, dev: &dyn WriteDevice, id: u64, off: u64, data: &[u8]) -> Result<()> {
        let bs = self.bs();
        let c = self.v.container();
        let end = off + data.len() as u64;
        let (first, last) = (off / bs, end.div_ceil(bs));
        let mut lb = first;
        while lb < last {
            let (phys, n, fresh_run) = match self.find_run(id, lb)? {
                Some(r) if r.fresh => (r.phys + (lb - r.lblock), (r.lblock + r.blocks).min(last) - lb, true),
                _ => {
                    // New blocks, up to the next fresh run.
                    let next_fresh = self.file(id)?.runs.iter().filter(|r| r.fresh && r.lblock > lb).map(|r| r.lblock).min();
                    let want = last.min(next_fresh.unwrap_or(u64::MAX)) - lb;
                    let (phys, n) = t.space.alloc_run(c, want)?;
                    if phys == 0 || phys + n > c.sb.block_count {
                        return Err(corrupt(format!("allocator returned blocks outside the container: {phys:#x}+{n}")));
                    }
                    self.st.data_alloced += n;
                    (phys, n, false)
                }
            };
            // Assemble the blocks: old contents at partial edges, then the new bytes.
            let mut buf = vec![0u8; (n * bs) as usize];
            let (bstart, bend) = (lb * bs, (lb + n) * bs);
            for edge in [lb, lb + n - 1] {
                let (es, ee) = (edge * bs, (edge + 1) * bs);
                if off > es || end < ee {
                    let mut old = vec![0u8; bs as usize];
                    if fresh_run {
                        self.read_fresh(phys + edge - lb, &mut old)?;
                    } else if let Some(r) = self.find_run(id, edge)? {
                        self.read_fresh(r.phys + edge - r.lblock, &mut old)?;
                    }
                    buf[(es - bstart) as usize..(ee - bstart) as usize].copy_from_slice(&old);
                }
            }
            let (s, e) = (off.max(bstart), end.min(bend));
            buf[(s - bstart) as usize..(e - bstart) as usize].copy_from_slice(&data[(s - off) as usize..(e - off) as usize]);
            self.stage(dev, phys, buf)?;
            if !fresh_run {
                self.map_run(t, id, Run { lblock: lb, phys, blocks: n, fresh: true })?;
            }
            lb += n;
        }
        Ok(())
    }

    /// Puts a fresh run into a file's block map, discarding what it covers.
    fn map_run(&mut self, t: &mut Txn, id: u64, new: Run) -> Result<()> {
        let f = self.file(id)?;
        let (a, b) = (new.lblock, new.lblock + new.blocks);
        let mut covered = Vec::new();
        let mut runs = Vec::with_capacity(f.runs.len() + 2);
        for r in f.runs.drain(..) {
            let (ra, rb) = (r.lblock, r.lblock + r.blocks);
            if rb <= a || ra >= b {
                runs.push(r);
                continue;
            }
            if ra < a {
                runs.push(Run { blocks: a - ra, ..r });
            }
            if rb > b {
                runs.push(Run { lblock: b, phys: r.phys + (b - ra), blocks: rb - b, fresh: r.fresh });
            }
            let (ca, cb) = (ra.max(a), rb.min(b));
            covered.push(Run { lblock: ca, phys: r.phys + (ca - ra), blocks: cb - ca, fresh: r.fresh });
        }
        let at = runs.partition_point(|r| r.lblock < a);
        runs.insert(at, new);
        // Merge with a fresh neighbour that continues on disk.
        if at > 0 {
            let p = runs[at - 1];
            if p.fresh && p.lblock + p.blocks == a && p.phys + p.blocks == new.phys {
                runs[at - 1].blocks += new.blocks;
                runs.remove(at);
            }
        }
        f.runs = runs;
        for r in covered {
            self.discard(t, r)?;
        }
        Ok(())
    }

    /// Lets go of a run no longer in a file's block map.
    fn discard(&mut self, t: &mut Txn, r: Run) -> Result<()> {
        if r.fresh {
            t.space.free_run(r.phys, r.blocks);
            self.st.data_freed += r.blocks;
            Ok(())
        } else {
            self.release_extent(t, r.phys, r.blocks)
        }
    }

    /// Rewrites file `id`'s extent records and data-stream field from its
    /// block map, if its data changed.
    fn flush_file(&mut self, id: u64) -> Result<()> {
        let bs = self.bs();
        let Some(f) = self.st.files.get_mut(&id) else { return Ok(()) };
        if !f.dirty {
            return Ok(());
        }
        f.dirty = false;
        let (stream, size, written) = (f.stream, f.size, f.written);
        let runs = f.runs.clone();
        // From here on the runs have extent references, like committed ones:
        // later changes go copy-on-write and release them properly.
        for r in &mut f.runs {
            r.fresh = false;
        }
        let (lo, hi) = fs::file_extent_range(stream);
        for (k, _) in self.range(&lo, &hi)? {
            self.remove(&k)?;
        }
        let c = self.v.container();
        // Extents must follow each other without gaps: a hole is an extent
        // at block 0.
        let mut alloced = 0;
        let mut next = 0;
        let end = size.div_ceil(bs);
        for r in &runs {
            if r.lblock > next {
                self.put(fs::file_extent_key(stream, next * bs), fs::file_extent_val((r.lblock - next) * bs, 0))?;
            }
            self.put(fs::file_extent_key(stream, r.lblock * bs), fs::file_extent_val(r.blocks * bs, r.phys))?;
            alloced += r.blocks * bs;
            next = r.lblock + r.blocks;
        }
        if end > next {
            self.put(fs::file_extent_key(stream, next * bs), fs::file_extent_val((end - next) * bs, 0))?;
        }
        let holes = end.max(next) * bs - alloced;
        // The data stream's allocated size spans the holes too.
        let alloced = end.max(next) * bs;
        // Extent references for fresh runs (committed runs keep theirs).
        let fresh: Vec<Run> = runs.iter().filter(|r| r.fresh).copied().collect();
        for r in fresh {
            let x = self.extref()?;
            x.upsert(&PhysReader(c), fs::phys_ext_key(r.phys), Some(fs::phys_ext_val(r.blocks, stream, 1)))?;
        }
        if self.get(&fs::dstream_id_key(stream))?.is_none() {
            self.put(fs::dstream_id_key(stream), fs::dstream_id_val(1))?;
        }
        let raw = self.inode_raw(id)?;
        let (mut fixed, mut fields) = fs::split_inode(&raw)?;
        let read = fields.iter().find(|f| f.0 == fs::INO_EXT_TYPE_DSTREAM).map_or(Ok(0), |d| u64_at(&d.2, 32))?;
        fs::set_xfield(&mut fields, fs::INO_EXT_TYPE_DSTREAM, fs::XF_SYSTEM_FIELD, fs::dstream(size, alloced, written, read));
        let mut flags = u64_at(&fixed, 48)?;
        if holes > 0 {
            flags |= fs::INODE_IS_SPARSE;
            fs::set_xfield(&mut fields, fs::INO_EXT_TYPE_SPARSE_BYTES, fs::XF_SYSTEM_FIELD | fs::XF_CHILDREN_INHERIT, holes.to_le_bytes().to_vec());
        } else {
            flags &= !fs::INODE_IS_SPARSE;
            fields.retain(|f| f.0 != fs::INO_EXT_TYPE_SPARSE_BYTES);
        }
        put_u64(&mut fixed, 48, flags);
        let now = super::now_ns();
        put_u64(&mut fixed, 24, now); // mod_time
        put_u64(&mut fixed, 32, now); // change_time
        self.put(fs::inode_key(id), fs::join_inode(&fixed, &fields))
    }

    /// Rewrites the records of every file whose data changed.
    pub fn flush_files(&mut self, dev: &dyn WriteDevice) -> Result<()> {
        self.write_pending_io(dev)?;
        let ids: Vec<u64> = self.st.files.keys().copied().collect();
        for id in ids {
            self.flush_file(id)?;
        }
        Ok(())
    }

    /// Releases all data of file `id` (stream `stream`) and its extent records.
    fn drop_file_data(&mut self, t: &mut Txn, id: u64, stream: u64) -> Result<()> {
        if let Some(f) = self.st.files.remove(&id) {
            for r in f.runs {
                self.discard(t, r)?;
            }
            let (lo, hi) = fs::file_extent_range(stream);
            for (k, _) in self.range(&lo, &hi)? {
                self.remove(&k)?;
            }
            return Ok(());
        }
        self.drop_extents(t, stream)
    }

    /// Removes a data stream's extent records, releasing their blocks.
    fn drop_extents(&mut self, t: &mut Txn, stream: u64) -> Result<()> {
        let bs = self.bs();
        let (lo, hi) = fs::file_extent_range(stream);
        for (k, v) in self.range(&lo, &hi)? {
            self.remove(&k)?;
            let v = v.ok_or_else(|| corrupt("file extent without a value"))?;
            let (len, phys) = (u64_at(&v, 0)? & fs::FILE_EXTENT_LEN_MASK, u64_at(&v, 8)?);
            if phys != 0 {
                // (Block 0 marks a hole in a sparse file.)
                self.release_extent(t, phys, len.div_ceil(bs))?;
            }
        }
        Ok(())
    }

    /// Frees `blocks` blocks from `phys` and takes them out of the
    /// extent-reference tree, splitting a record that covers more.
    fn release_extent(&mut self, t: &mut Txn, phys: u64, blocks: u64) -> Result<()> {
        let rd = PhysReader(self.v.container());
        let x = self.extref()?;
        let (k, v) = x
            .floor(&rd, &fs::phys_ext_key(phys))?
            .ok_or_else(|| corrupt(format!("no extent reference for block {phys:#x}")))?;
        let v = v.ok_or_else(|| corrupt("extent reference without a value"))?;
        let start = fs::key_id(&k);
        let (rlen, owner, refcnt) = (u64_at(&v, 0)? & fs::PEXT_LEN_MASK, u64_at(&v, 8)?, u32_at(&v, 16)? as i32);
        if start + rlen < phys + blocks {
            return Err(corrupt(format!("blocks {phys:#x}+{blocks} are not covered by an extent reference")));
        }
        if refcnt != 1 {
            return Err(unsupported("changing data shared with clones or snapshots"));
        }
        x.remove(&rd, &k)?;
        if start < phys {
            x.upsert(&rd, fs::phys_ext_key(start), Some(fs::phys_ext_val(phys - start, owner, 1)))?;
        }
        if phys + blocks < start + rlen {
            let rest = start + rlen - phys - blocks;
            x.upsert(&rd, fs::phys_ext_key(phys + blocks), Some(fs::phys_ext_val(rest, owner, 1)))?;
        }
        t.space.free_run(phys, blocks);
        self.st.data_freed += blocks;
        Ok(())
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 255 || name.contains(['/', '\0']) || name == "." || name == ".." {
        return Err(Error::InvalidName(format!("{name:?}")));
    }
    Ok(())
}
