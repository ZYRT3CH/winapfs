//! A volume's file-system tree: inodes, directory entries, extended
//! attributes and file extents.

use std::cmp::Ordering;
use std::io::Write;
use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;

use crate::btree::{self, BTreeInfo, Node, NodeSource};
use crate::bytes::{cstr, slice_at, u16_at, u32_at, u64_at};
use crate::decmpfs;
use crate::error::{corrupt, unsupported, Error, Result};
use crate::nx::{ApfsSuperblock, Container, APFS_INCOMPAT_NORMALIZATION_INSENSITIVE};
use crate::omap::{Omap, OMAP_VAL_ENCRYPTED};

pub(crate) const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
const OBJ_TYPE_SHIFT: u32 = 60;

pub const APFS_TYPE_INODE: u8 = 3;
pub const APFS_TYPE_XATTR: u8 = 4;
pub const APFS_TYPE_FILE_EXTENT: u8 = 8;
pub const APFS_TYPE_DIR_REC: u8 = 9;

/// Parent of the root and private directories; never exists as an inode.
pub const ROOT_DIR_PARENT: u64 = 1;
pub const ROOT_DIR_INO_NUM: u64 = 2;
/// Hidden directory for internal files; like the root, not counted in `apfs_num_directories`.
pub const PRIV_DIR_INO_NUM: u64 = 3;

const UF_COMPRESSED: u32 = 0x20;
const INO_EXT_TYPE_NAME: u8 = 4;
const INO_EXT_TYPE_DSTREAM: u8 = 8;
const XATTR_DATA_STREAM: u16 = 0x1;
const J_FILE_EXTENT_LEN_MASK: u64 = 0x00ff_ffff_ffff_ffff;
const DREC_TYPE_MASK: u16 = 0x000f;
const SYMLINK_XATTR: &str = "com.apple.fs.symlink";

/// Copy granularity for file data; a multiple of every legal block size.
const COPY_CHUNK: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Fifo,
    CharDevice,
    Directory,
    BlockDevice,
    Regular,
    Symlink,
    Socket,
    Whiteout,
    Unknown(u16),
}

impl FileKind {
    /// From a `DT_*` value; `S_IFMT >> 12` uses the same numbering.
    pub fn from_dt(dt: u16) -> Self {
        match dt {
            1 => Self::Fifo,
            2 => Self::CharDevice,
            4 => Self::Directory,
            6 => Self::BlockDevice,
            8 => Self::Regular,
            10 => Self::Symlink,
            12 => Self::Socket,
            14 => Self::Whiteout,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Dstream {
    pub size: u64,
    pub alloced_size: u64,
}

#[derive(Debug, Clone)]
pub struct Inode {
    pub id: u64,
    pub parent_id: u64,
    /// Object id of the file's data stream.
    pub private_id: u64,
    pub create_time: u64,
    pub mod_time: u64,
    pub change_time: u64,
    pub access_time: u64,
    pub internal_flags: u64,
    /// Child count for directories, link count for everything else.
    pub nchildren_or_nlink: i32,
    pub bsd_flags: u32,
    pub owner: u32,
    pub group: u32,
    pub mode: u16,
    pub name: Option<String>,
    pub dstream: Option<Dstream>,
}

impl Inode {
    pub(crate) fn parse(id: u64, v: &[u8]) -> Result<Self> {
        let mut inode = Self {
            id,
            parent_id: u64_at(v, 0)?,
            private_id: u64_at(v, 8)?,
            create_time: u64_at(v, 16)?,
            mod_time: u64_at(v, 24)?,
            change_time: u64_at(v, 32)?,
            access_time: u64_at(v, 40)?,
            internal_flags: u64_at(v, 48)?,
            nchildren_or_nlink: u32_at(v, 56)? as i32,
            bsd_flags: u32_at(v, 68)?,
            owner: u32_at(v, 72)?,
            group: u32_at(v, 76)?,
            mode: u16_at(v, 80)?,
            name: None,
            dstream: None,
        };
        for (kind, data) in xfields(v.get(92..).unwrap_or(&[]))? {
            match kind {
                INO_EXT_TYPE_NAME => inode.name = Some(cstr(data)),
                INO_EXT_TYPE_DSTREAM => {
                    inode.dstream = Some(Dstream { size: u64_at(data, 0)?, alloced_size: u64_at(data, 8)? })
                }
                _ => {}
            }
        }
        Ok(inode)
    }

    pub fn kind(&self) -> FileKind {
        FileKind::from_dt(self.mode >> 12)
    }

    pub fn is_compressed(&self) -> bool {
        self.bsd_flags & UF_COMPRESSED != 0
    }
}

/// Parses an `xf_blob_t`: a count, a table of field headers, then each
/// field's data padded to 8 bytes.
fn xfields(blob: &[u8]) -> Result<Vec<(u8, &[u8])>> {
    if blob.len() < 4 {
        return Ok(Vec::new());
    }
    let n = u16_at(blob, 0)? as usize;
    let mut out = Vec::with_capacity(n);
    let mut data_off = 4 + 4 * n;
    for i in 0..n {
        let hdr = slice_at(blob, 4 + 4 * i, 4)?;
        let size = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
        out.push((hdr[0], slice_at(blob, data_off, size)?));
        data_off += size.div_ceil(8) * 8;
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub file_id: u64,
    /// Nanoseconds since 1970-01-01 UTC.
    pub date_added: u64,
    pub kind: FileKind,
}

#[derive(Debug, Clone)]
pub enum XattrData {
    Embedded(Vec<u8>),
    Stream { obj_id: u64, size: u64 },
}

#[derive(Debug, Clone)]
pub struct Xattr {
    pub name: String,
    pub flags: u16,
    pub data: XattrData,
}

/// One run of a file's data: `len` bytes at logical offset `logical`,
/// stored from block `phys_block` (0 means a hole).
#[derive(Debug, Clone, Copy)]
pub struct Extent {
    pub logical: u64,
    pub len: u64,
    pub phys_block: u64,
}

pub struct Volume<'c> {
    c: &'c Container,
    pub sb: ApfsSuperblock,
    omap: Omap,
    root: Arc<Node>,
    root_info: BTreeInfo,
}

impl<'c> Volume<'c> {
    pub(crate) fn open(c: &'c Container, sb: ApfsSuperblock) -> Result<Self> {
        if sb.is_encrypted() {
            return Err(unsupported(format!("volume \"{}\" is encrypted", sb.name)));
        }
        if sb.is_sealed() {
            return Err(unsupported(format!("volume \"{}\" is a sealed system volume", sb.name)));
        }
        let omap = Omap::load(&c.r, &c.r.read_object(sb.omap_oid)?)?;
        let root_paddr = resolve(c, &omap, sb.root_tree_oid)?;
        let root = c.r.read_node(root_paddr)?;
        let root_info = root.info()?;
        Ok(Self { c, sb, omap, root, root_info })
    }

    pub fn root(&self) -> Result<Inode> {
        self.inode(ROOT_DIR_INO_NUM)
    }

    pub fn inode(&self, id: u64) -> Result<Inode> {
        let mut out = None;
        self.records(id, APFS_TYPE_INODE, &mut |_, v| {
            out = Some(Inode::parse(id, v)?);
            Ok(false)
        })?;
        out.ok_or_else(|| Error::NotFound(format!("inode {id}")))
    }

    pub fn read_dir(&self, dir_id: u64) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        self.records(dir_id, APFS_TYPE_DIR_REC, &mut |k, v| {
            out.push(DirEntry {
                name: drec_name(k)?,
                file_id: u64_at(v, 0)?,
                date_added: u64_at(v, 8)?,
                kind: FileKind::from_dt(u16_at(v, 16)? & DREC_TYPE_MASK),
            });
            Ok(true)
        })?;
        Ok(out)
    }

    /// Resolves a `/`- or `\`-separated path from the volume root. Matching
    /// follows the volume: normalization-insensitive, and case-insensitive
    /// when the volume is.
    pub fn lookup_path(&self, path: &str) -> Result<Inode> {
        let mut cur = self.root()?;
        for comp in path.split(['/', '\\']).filter(|c| !c.is_empty()) {
            let entries = self.read_dir(cur.id)?;
            let hit = match entries.iter().find(|e| e.name == comp) {
                Some(e) => Some(e),
                None => {
                    let want = self.name_key(comp);
                    entries.iter().find(|e| self.name_key(&e.name) == want)
                }
            };
            let hit = hit.ok_or_else(|| Error::NotFound(format!("\"{comp}\" in {path}")))?;
            cur = self.inode(hit.file_id)?;
        }
        Ok(cur)
    }

    /// Two names refer to the same entry on this volume exactly when their
    /// keys are equal.
    pub fn name_key(&self, name: &str) -> String {
        let nfd = if self.sb.incompat_features & APFS_INCOMPAT_NORMALIZATION_INSENSITIVE != 0 || self.sb.is_case_insensitive() {
            name.nfd().collect::<String>()
        } else {
            name.to_string()
        };
        if self.sb.is_case_insensitive() { nfd.to_lowercase() } else { nfd }
    }

    pub fn xattrs(&self, id: u64) -> Result<Vec<Xattr>> {
        let mut out = Vec::new();
        self.records(id, APFS_TYPE_XATTR, &mut |k, v| {
            let name_len = u16_at(k, 8)? as usize;
            let flags = u16_at(v, 0)?;
            let len = u16_at(v, 2)? as usize;
            let data = slice_at(v, 4, len)?;
            let data = if flags & XATTR_DATA_STREAM != 0 {
                // j_xattr_dstream_t: object id, then a j_dstream_t whose first field is the size.
                XattrData::Stream { obj_id: u64_at(data, 0)?, size: u64_at(data, 8)? }
            } else {
                XattrData::Embedded(data.to_vec())
            };
            out.push(Xattr { name: cstr(slice_at(k, 10, name_len)?), flags, data });
            Ok(true)
        })?;
        Ok(out)
    }

    /// The volume object map's entries as (oid, xid, value).
    pub fn omap_entries(&self) -> Result<Vec<(u64, u64, crate::omap::OmapVal)>> {
        self.omap.entries(&self.c.r)
    }

    /// The file-system tree's root node.
    pub fn fs_root(&self) -> &Node {
        &self.root
    }

    pub fn xattr(&self, id: u64, name: &str) -> Result<Option<Xattr>> {
        Ok(self.xattrs(id)?.into_iter().find(|x| x.name == name))
    }

    pub fn read_xattr(&self, x: &Xattr) -> Result<Vec<u8>> {
        match &x.data {
            XattrData::Embedded(d) => Ok(d.clone()),
            XattrData::Stream { obj_id, size } => {
                let mut out = Vec::with_capacity(*size as usize);
                self.copy_dstream(*obj_id, *size, &mut out)?;
                Ok(out)
            }
        }
    }

    /// Logical size of a file's contents, after decompression.
    pub fn file_size(&self, inode: &Inode) -> Result<u64> {
        if inode.is_compressed() {
            let x = self.decmpfs_xattr(inode)?;
            return Ok(decmpfs::header(&self.read_xattr(&x)?)?.size);
        }
        Ok(inode.dstream.map_or(0, |d| d.size))
    }

    /// Streams a regular file's contents into `w`, decompressing if needed.
    pub fn read_file(&self, inode: &Inode, w: &mut dyn Write) -> Result<u64> {
        if inode.is_compressed() {
            let x = self.decmpfs_xattr(inode)?;
            let mut rsrc = || -> Result<Vec<u8>> {
                let fork = self
                    .xattr(inode.id, decmpfs::RESOURCE_FORK_XATTR)?
                    .ok_or_else(|| corrupt(format!("inode {} is missing its compressed resource fork", inode.id)))?;
                self.read_xattr(&fork)
            };
            return decmpfs::decompress(&self.read_xattr(&x)?, &mut rsrc, w);
        }
        match inode.dstream {
            Some(d) => self.copy_dstream(inode.private_id, d.size, w),
            None => Ok(0),
        }
    }

    pub fn read_link(&self, inode: &Inode) -> Result<String> {
        let x = self
            .xattr(inode.id, SYMLINK_XATTR)?
            .ok_or_else(|| corrupt(format!("symlink inode {} has no target", inode.id)))?;
        Ok(cstr(&self.read_xattr(&x)?))
    }

    fn decmpfs_xattr(&self, inode: &Inode) -> Result<Xattr> {
        self.xattr(inode.id, decmpfs::DECMPFS_XATTR)?
            .ok_or_else(|| corrupt(format!("compressed inode {} has no decmpfs attribute", inode.id)))
    }

    pub(crate) fn extents(&self, obj_id: u64) -> Result<Vec<Extent>> {
        let mut out = Vec::new();
        self.records(obj_id, APFS_TYPE_FILE_EXTENT, &mut |k, v| {
            out.push(Extent {
                logical: u64_at(k, 8)?,
                len: u64_at(v, 0)? & J_FILE_EXTENT_LEN_MASK,
                phys_block: u64_at(v, 8)?,
            });
            Ok(true)
        })?;
        Ok(out)
    }

    fn copy_dstream(&self, obj_id: u64, size: u64, w: &mut dyn Write) -> Result<u64> {
        let bs = self.c.block_size() as u64;
        let mut buf = vec![0u8; COPY_CHUNK];
        let mut pos = 0u64;
        for e in self.extents(obj_id)? {
            if pos >= size {
                break;
            }
            if e.logical < pos {
                return Err(corrupt(format!("overlapping extents in data stream {obj_id:#x}")));
            }
            write_zeros(w, (e.logical - pos).min(size - pos))?; // sparse gap
            pos = e.logical.min(size);
            let want = e.len.min(size - pos);
            if e.phys_block == 0 {
                write_zeros(w, want)?;
            } else {
                if e.phys_block + e.len.div_ceil(bs) > self.c.r.block_count {
                    return Err(corrupt(format!("extent of data stream {obj_id:#x} runs past the container")));
                }
                let mut done = 0u64;
                while done < want {
                    let n = (want - done).min(COPY_CHUNK as u64);
                    let chunk = &mut buf[..(n.div_ceil(bs) * bs) as usize];
                    self.c.r.read_at((e.phys_block + done / bs) * bs, chunk)?;
                    w.write_all(&chunk[..n as usize])?;
                    done += n;
                }
            }
            pos += want;
        }
        write_zeros(w, size - pos)?;
        Ok(size)
    }

    /// Reads up to `buf.len()` bytes of an uncompressed file starting at
    /// `off`; returns how many bytes were filled (0 at end of file). Holes read
    /// as zeros.
    pub fn read_at(&self, inode: &Inode, off: u64, buf: &mut [u8]) -> Result<usize> {
        if inode.is_compressed() {
            return Err(unsupported("random access into compressed files (use read_file)"));
        }
        self.read_extents_at(inode, &self.extents(inode.private_id)?, off, buf)
    }

    /// The extents of an uncompressed file, in logical order, for repeated
    /// [`Volume::read_extents_at`] calls.
    pub fn file_extents(&self, inode: &Inode) -> Result<Vec<Extent>> {
        if inode.is_compressed() {
            return Err(unsupported("extents of compressed files"));
        }
        self.extents(inode.private_id)
    }

    /// [`Volume::read_at`] with the file's extents already fetched.
    pub fn read_extents_at(&self, inode: &Inode, extents: &[Extent], off: u64, buf: &mut [u8]) -> Result<usize> {
        let size = inode.dstream.map_or(0, |d| d.size);
        if off >= size {
            return Ok(0);
        }
        let len = (buf.len() as u64).min(size - off);
        let end = off + len;
        let buf = &mut buf[..len as usize];
        buf.fill(0);
        let bs = self.c.block_size() as u64;
        // Extents are sorted by logical offset; start at the last one beginning at or before `off`.
        let first_ext = extents.partition_point(|e| e.logical <= off).saturating_sub(1);
        for e in &extents[first_ext..] {
            if e.logical >= end {
                break;
            }
            let (start, stop) = (off.max(e.logical), end.min(e.logical + e.len));
            if start >= stop || e.phys_block == 0 {
                continue;
            }
            if e.phys_block + e.len.div_ceil(bs) > self.c.r.block_count {
                return Err(corrupt(format!("extent of inode {} runs past the container", inode.id)));
            }
            let first = (start - e.logical) / bs;
            let last = (stop - e.logical).div_ceil(bs);
            let mut tmp = vec![0u8; ((last - first) * bs) as usize];
            self.c.r.read_at((e.phys_block + first) * bs, &mut tmp)?;
            let skip = (start - e.logical - first * bs) as usize;
            buf[(start - off) as usize..(stop - off) as usize].copy_from_slice(&tmp[skip..skip + (stop - start) as usize]);
        }
        Ok(len as usize)
    }

    pub(crate) fn container(&self) -> &'c Container {
        self.c
    }

    /// A file-system tree node by object id, and the block it was read from.
    pub(crate) fn fs_node(&self, oid: u64) -> Result<(Arc<Node>, u64)> {
        let paddr = resolve(self.c, &self.omap, oid)?;
        Ok((self.c.r.read_node(paddr)?, paddr))
    }

    /// Visits every record in the file-system tree, in key order.
    pub fn scan_all(&self, f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>) -> Result<()> {
        let resolve = |oid: u64| resolve(self.c, &self.omap, oid);
        btree::scan(&self.c.r, &self.root, &self.root_info, &resolve, &|_| Ok(Ordering::Equal), f)
    }

    /// Visits every record of object `id` with the given record type.
    fn records(&self, id: u64, rtype: u8, f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>) -> Result<()> {
        let want = (id, rtype);
        let window = |k: &[u8]| -> Result<Ordering> {
            let hdr = u64_at(k, 0)?;
            Ok((hdr & OBJ_ID_MASK, (hdr >> OBJ_TYPE_SHIFT) as u8).cmp(&want))
        };
        let resolve = |oid: u64| resolve(self.c, &self.omap, oid);
        btree::scan(&self.c.r, &self.root, &self.root_info, &resolve, &window, f)
    }
}

/// Physical address of the newest version of a volume's virtual object.
fn resolve(c: &Container, omap: &Omap, oid: u64) -> Result<u64> {
    let v = omap
        .lookup(&c.r, oid, c.xid())?
        .ok_or_else(|| corrupt(format!("virtual object {oid:#x} missing from the volume object map")))?;
    if v.flags & OMAP_VAL_ENCRYPTED != 0 {
        return Err(unsupported("encrypted file-system tree nodes"));
    }
    Ok(v.paddr)
}

/// Directory-entry names come in a hashed form (`j_drec_hashed_key_t`, a
/// 10-bit length and 22-bit hash) or a plain one (`j_drec_key_t`, a 16-bit
/// length). Both include a trailing NUL; tell them apart by key length.
fn drec_name(k: &[u8]) -> Result<String> {
    let hashed_len = (u32_at(k, 8)? & 0x3ff) as usize;
    if k.len() == 12 + hashed_len {
        return Ok(cstr(&k[12..]));
    }
    let plain_len = u16_at(k, 8)? as usize;
    if k.len() == 10 + plain_len {
        return Ok(cstr(&k[10..]));
    }
    Err(corrupt(format!("directory entry key of {} bytes matches neither key format", k.len())))
}

fn write_zeros(w: &mut dyn Write, mut n: u64) -> Result<()> {
    const ZEROS: [u8; 65536] = [0; 65536];
    while n > 0 {
        let k = n.min(ZEROS.len() as u64) as usize;
        w.write_all(&ZEROS[..k])?;
        n -= k as u64;
    }
    Ok(())
}
