//! Building file-system records, and their order in the tree.

use std::cmp::Ordering;

use crate::error::{corrupt, Result};
use crate::name_hash::name_len_and_hash;

const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
const APFS_TYPE_EXTENT: u64 = 2;
const APFS_TYPE_INODE: u64 = 3;
const APFS_TYPE_XATTR: u64 = 4;
const APFS_TYPE_DSTREAM_ID: u64 = 6;
const APFS_TYPE_FILE_EXTENT: u64 = 8;
const APFS_TYPE_DIR_REC: u64 = 9;

pub const DT_REG: u16 = 8;
pub const DT_DIR: u16 = 4;

pub const INODE_IS_SPARSE: u64 = 0x200;
const INODE_NO_RSRC_FORK: u64 = 0x8000;
pub const INO_EXT_TYPE_NAME: u8 = 4;
pub const INO_EXT_TYPE_DSTREAM: u8 = 8;
pub const INO_EXT_TYPE_SPARSE_BYTES: u8 = 13;
pub const XF_DO_NOT_COPY: u8 = 0x02;
pub const XF_SYSTEM_FIELD: u8 = 0x20;
pub const UF_COMPRESSED: u32 = 0x20;
const S_IFMT: u16 = 0o170000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFDIR: u16 = 0o040000;
/// Owner and group 99 ("unknown"): macOS shows files on external drives
/// with this owner as belonging to whoever is logged in.
const UNKNOWN_ID: u32 = 99;

pub const FILE_EXTENT_LEN_MASK: u64 = 0x00ff_ffff_ffff_ffff;
pub const PEXT_LEN_MASK: u64 = 0x0fff_ffff_ffff_ffff;
const APFS_KIND_NEW: u64 = 1;

/// Fixed part of `j_inode_val_t`; extended fields follow.
pub const INODE_FIXED: usize = 92;

fn hdr(id: u64, kind: u64) -> [u8; 8] {
    ((id & OBJ_ID_MASK) | kind << 60).to_le_bytes()
}

/// The object id in a key's header.
pub fn key_id(key: &[u8]) -> u64 {
    u64::from_le_bytes(key[0..8].try_into().unwrap()) & OBJ_ID_MASK
}

pub fn inode_key(id: u64) -> Vec<u8> {
    hdr(id, APFS_TYPE_INODE).to_vec()
}

/// A hashed directory-entry key (volumes formatted by macOS 10.13 and later).
pub fn drec_key(dir: u64, name: &str, case_insensitive: bool) -> Vec<u8> {
    let mut k = hdr(dir, APFS_TYPE_DIR_REC).to_vec();
    k.extend(name_len_and_hash(name, case_insensitive).to_le_bytes());
    k.extend(name.as_bytes());
    k.push(0);
    k
}

pub fn drec_val(file_id: u64, date_added: u64, kind: u16) -> Vec<u8> {
    let mut v = file_id.to_le_bytes().to_vec();
    v.extend(date_added.to_le_bytes());
    v.extend(kind.to_le_bytes());
    v
}

/// Lowest and past-the-end keys of the directory entries in `dir` whose
/// names hash like `name`. Names that are the same after normalization (and
/// case folding) hash alike, though their lengths may differ.
pub fn drec_hash_range(dir: u64, name: &str, case_insensitive: bool) -> (Vec<u8>, Vec<u8>) {
    let hash = name_len_and_hash(name, case_insensitive) >> 10;
    let mut lo = hdr(dir, APFS_TYPE_DIR_REC).to_vec();
    lo.extend((hash << 10).to_le_bytes());
    let hi = if hash < 0x3f_ffff {
        let mut hi = hdr(dir, APFS_TYPE_DIR_REC).to_vec();
        hi.extend(((hash + 1) << 10).to_le_bytes());
        hi
    } else {
        hdr(dir, APFS_TYPE_DIR_REC + 1).to_vec()
    };
    (lo, hi)
}

/// The name in a hashed directory-entry key.
pub fn drec_name(key: &[u8]) -> String {
    let name = &key[12..];
    String::from_utf8_lossy(name.strip_suffix(&[0]).unwrap_or(name)).into_owned()
}

/// `j_file_extent_key_t`: the extent of data stream `stream` at byte `logical`.
pub fn file_extent_key(stream: u64, logical: u64) -> Vec<u8> {
    let mut k = hdr(stream, APFS_TYPE_FILE_EXTENT).to_vec();
    k.extend(logical.to_le_bytes());
    k
}

/// Lowest and past-the-end keys of a data stream's extents.
pub fn file_extent_range(stream: u64) -> (Vec<u8>, Vec<u8>) {
    (file_extent_key(stream, 0), hdr(stream, APFS_TYPE_FILE_EXTENT + 1).to_vec())
}

/// `j_file_extent_val_t`: `len` bytes (a whole number of blocks) at block `phys`.
pub fn file_extent_val(len: u64, phys: u64) -> Vec<u8> {
    let mut v = len.to_le_bytes().to_vec(); // no flags
    v.extend(phys.to_le_bytes());
    v.extend(0u64.to_le_bytes()); // crypto_id
    v
}

/// `j_phys_ext_key_t`, in the volume's extent-reference tree.
pub fn phys_ext_key(start: u64) -> Vec<u8> {
    hdr(start, APFS_TYPE_EXTENT).to_vec()
}

/// `j_phys_ext_val_t`: `blocks` blocks used once, by data stream `owner`.
pub fn phys_ext_val(blocks: u64, owner: u64, refcnt: i32) -> Vec<u8> {
    let mut v = (blocks | APFS_KIND_NEW << 60).to_le_bytes().to_vec();
    v.extend(owner.to_le_bytes());
    v.extend(refcnt.to_le_bytes());
    v
}

pub fn dstream_id_key(stream: u64) -> Vec<u8> {
    hdr(stream, APFS_TYPE_DSTREAM_ID).to_vec()
}

/// `j_dstream_id_val_t`: how many files use the data stream.
pub fn dstream_id_val(refcnt: u32) -> Vec<u8> {
    refcnt.to_le_bytes().to_vec()
}

/// `j_dstream_t`.
pub fn dstream(size: u64, alloced: u64, written: u64, read: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(40);
    for x in [size, alloced, 0 /* default_crypto_id */, written, read] {
        v.extend(x.to_le_bytes());
    }
    v
}

/// An inode's extended field: type, flags, data.
pub type Xfield = (u8, u8, Vec<u8>);

/// Splits a `j_inode_val_t` into its fixed part and its extended fields.
pub fn split_inode(v: &[u8]) -> Result<(Vec<u8>, Vec<Xfield>)> {
    let fixed = v.get(..INODE_FIXED).ok_or_else(|| corrupt("inode record too short"))?.to_vec();
    let blob = &v[INODE_FIXED..];
    let mut fields = Vec::new();
    if blob.len() >= 4 {
        let n = u16::from_le_bytes([blob[0], blob[1]]) as usize;
        let mut data = 4 + 4 * n;
        for i in 0..n {
            let h = blob.get(4 + 4 * i..8 + 4 * i).ok_or_else(|| corrupt("inode extended fields"))?;
            let size = u16::from_le_bytes([h[2], h[3]]) as usize;
            let d = blob.get(data..data + size).ok_or_else(|| corrupt("inode extended field data"))?;
            fields.push((h[0], h[1], d.to_vec()));
            data += size.div_ceil(8) * 8;
        }
    }
    Ok((fixed, fields))
}

/// Joins a fixed inode part and extended fields (`xf_blob_t`, each field's
/// data padded to 8 bytes) back into a `j_inode_val_t`.
pub fn join_inode(fixed: &[u8], fields: &[Xfield]) -> Vec<u8> {
    let mut v = fixed.to_vec();
    if fields.is_empty() {
        return v;
    }
    let used: usize = fields.iter().map(|f| f.2.len().div_ceil(8) * 8).sum();
    v.extend((fields.len() as u16).to_le_bytes());
    v.extend((used as u16).to_le_bytes());
    for (kind, flags, data) in fields {
        v.push(*kind);
        v.push(*flags);
        v.extend((data.len() as u16).to_le_bytes());
    }
    for (_, _, data) in fields {
        v.extend(data);
        v.resize(v.len() + data.len().div_ceil(8) * 8 - data.len(), 0);
    }
    v
}

/// Sets field `kind`, keeping fields in type order.
pub fn set_xfield(fields: &mut Vec<Xfield>, kind: u8, flags: u8, data: Vec<u8>) {
    match fields.iter().position(|f| f.0 >= kind) {
        Some(i) if fields[i].0 == kind => fields[i] = (kind, flags, data),
        Some(i) => fields.insert(i, (kind, flags, data)),
        None => fields.push((kind, flags, data)),
    }
}

fn mode(inode: &[u8]) -> u16 {
    if inode.len() >= 82 {
        u16::from_le_bytes([inode[80], inode[81]])
    } else {
        0
    }
}

pub fn is_dir(inode: &[u8]) -> bool {
    mode(inode) & S_IFMT == S_IFDIR
}

pub fn is_regular(inode: &[u8]) -> bool {
    mode(inode) & S_IFMT == S_IFREG
}

/// `j_inode_val_t` for a new, empty file (`S_IFREG`) or folder (`S_IFDIR`).
pub fn new_inode(parent: u64, id: u64, name: &str, now: u64, kind: u16) -> Vec<u8> {
    let perms = if kind == S_IFDIR { 0o755 } else { 0o644 };
    let mut v = Vec::with_capacity(128);
    v.extend(parent.to_le_bytes());
    v.extend(id.to_le_bytes()); // private_id: the data stream id; the inode's own id while it has none
    for _ in 0..4 {
        v.extend(now.to_le_bytes()); // create, mod, change, access
    }
    v.extend(INODE_NO_RSRC_FORK.to_le_bytes());
    v.extend(i32::from(kind != S_IFDIR).to_le_bytes()); // a file's link count, a folder's child count
    v.extend(0u32.to_le_bytes()); // default_protection_class
    v.extend(0u32.to_le_bytes()); // write_generation_counter
    v.extend(0u32.to_le_bytes()); // bsd_flags
    v.extend(UNKNOWN_ID.to_le_bytes()); // owner
    v.extend(UNKNOWN_ID.to_le_bytes()); // group
    v.extend((kind | perms).to_le_bytes());
    v.extend(0u16.to_le_bytes()); // pad1
    v.extend(0u64.to_le_bytes()); // uncompressed_size
    let mut name_z = name.as_bytes().to_vec();
    name_z.push(0);
    join_inode(&v, &[(INO_EXT_TYPE_NAME, XF_DO_NOT_COPY, name_z)])
}

fn u32_or_0(k: &[u8], off: usize) -> u32 {
    k.get(off..off + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn u64_or_0(k: &[u8], off: usize) -> u64 {
    k.get(off..off + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
}

/// File-system key order: object id, record type, then per type: the
/// hash-and-length word and name for hashed directory entries, the name for
/// extended attributes, the logical offset for file extents. Missing parts
/// of a search bound sort first.
pub fn key_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (ha, hb) = (u64_or_0(a, 0), u64_or_0(b, 0));
    let primary = (ha & OBJ_ID_MASK, ha >> 60).cmp(&(hb & OBJ_ID_MASK, hb >> 60));
    if primary != Ordering::Equal {
        return primary;
    }
    fn tail(k: &[u8], from: usize) -> &[u8] {
        k.get(from..).unwrap_or(&[])
    }
    match ha >> 60 {
        APFS_TYPE_DIR_REC => u32_or_0(a, 8).cmp(&u32_or_0(b, 8)).then_with(|| tail(a, 12).cmp(&tail(b, 12))),
        APFS_TYPE_XATTR => tail(a, 10).cmp(&tail(b, 10)),
        APFS_TYPE_FILE_EXTENT => u64_or_0(a, 8).cmp(&u64_or_0(b, 8)),
        _ => tail(a, 8).cmp(&tail(b, 8)),
    }
}
