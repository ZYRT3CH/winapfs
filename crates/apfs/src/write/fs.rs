//! Building file-system records, and their order in the tree.

use std::cmp::Ordering;

use crate::btree_write::Entry;
use crate::name_hash::name_len_and_hash;

const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
const APFS_TYPE_INODE: u64 = 3;
const APFS_TYPE_XATTR: u64 = 4;
const APFS_TYPE_FILE_EXTENT: u64 = 8;
const APFS_TYPE_DIR_REC: u64 = 9;

pub const DT_REG: u16 = 8;
pub const DT_DIR: u16 = 4;

const INODE_NO_RSRC_FORK: u64 = 0x8000;
const INO_EXT_TYPE_NAME: u8 = 4;
const XF_DO_NOT_COPY: u8 = 0x02;
const S_IFREG: u16 = 0o100000;
/// Owner and group 99 ("unknown"): macOS shows files on external drives
/// with this owner as belonging to whoever is logged in.
const UNKNOWN_ID: u32 = 99;

fn hdr(id: u64, kind: u64) -> [u8; 8] {
    ((id & OBJ_ID_MASK) | kind << 60).to_le_bytes()
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

/// An `xf_blob_t` holding just the inode's name.
fn name_xfield(name: &str) -> Vec<u8> {
    let data_len = name.len() + 1;
    let mut x = 1u16.to_le_bytes().to_vec(); // xf_num_exts
    x.extend(((data_len.div_ceil(8) * 8) as u16).to_le_bytes()); // xf_used_data
    x.push(INO_EXT_TYPE_NAME);
    x.push(XF_DO_NOT_COPY);
    x.extend((data_len as u16).to_le_bytes());
    x.extend(name.as_bytes());
    x.push(0);
    x.resize(4 + 4 + data_len.div_ceil(8) * 8, 0);
    x
}

/// `j_inode_val_t` for a new, empty regular file.
pub fn new_file_inode(parent: u64, id: u64, name: &str, now: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(128);
    v.extend(parent.to_le_bytes());
    v.extend(id.to_le_bytes()); // private_id: the data stream id; the inode's own id while it has none
    for _ in 0..4 {
        v.extend(now.to_le_bytes()); // create, mod, change, access
    }
    v.extend(INODE_NO_RSRC_FORK.to_le_bytes());
    v.extend(1i32.to_le_bytes()); // nlink
    v.extend(0u32.to_le_bytes()); // default_protection_class
    v.extend(0u32.to_le_bytes()); // write_generation_counter
    v.extend(0u32.to_le_bytes()); // bsd_flags
    v.extend(UNKNOWN_ID.to_le_bytes()); // owner
    v.extend(UNKNOWN_ID.to_le_bytes()); // group
    v.extend((S_IFREG | 0o644).to_le_bytes());
    v.extend(0u16.to_le_bytes()); // pad1
    v.extend(0u64.to_le_bytes()); // uncompressed_size
    v.extend(name_xfield(name));
    v
}

/// File-system key order: object id, record type, then per type: the
/// hash-and-length word and name for hashed directory entries, the name for
/// extended attributes, the logical offset for file extents.
pub fn key_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let ha = u64::from_le_bytes(a[0..8].try_into().unwrap());
    let hb = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let primary = (ha & OBJ_ID_MASK, ha >> 60).cmp(&(hb & OBJ_ID_MASK, hb >> 60));
    if primary != Ordering::Equal {
        return primary;
    }
    match ha >> 60 {
        APFS_TYPE_DIR_REC => {
            let wa = u32::from_le_bytes(a[8..12].try_into().unwrap());
            let wb = u32::from_le_bytes(b[8..12].try_into().unwrap());
            wa.cmp(&wb).then_with(|| a[12..].cmp(&b[12..]))
        }
        APFS_TYPE_XATTR => a[10..].cmp(&b[10..]),
        APFS_TYPE_FILE_EXTENT => u64::from_le_bytes(a[8..16].try_into().unwrap())
            .cmp(&u64::from_le_bytes(b[8..16].try_into().unwrap())),
        _ => a[8..].cmp(&b[8..]),
    }
}

/// Inserts a record at its sorted position (replacing an equal key).
pub fn insert(entries: &mut Vec<Entry>, key: Vec<u8>, val: Vec<u8>) {
    match entries.binary_search_by(|(k, _)| key_cmp(k, &key)) {
        Ok(i) => entries[i].1 = Some(val),
        Err(i) => entries.insert(i, (key, Some(val))),
    }
}
