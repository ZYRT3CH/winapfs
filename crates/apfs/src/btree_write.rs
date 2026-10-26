//! Serialising B-tree nodes. Writes are copy-on-write, so a node is always
//! built whole from its entries: keys packed from the start of the key area,
//! values from the end of the value area, no free lists.

use crate::btree::{BTreeInfo, BTNODE_FIXED_KV_SIZE, BTNODE_LEAF, BTNODE_ROOT, BTREE_INFO_SIZE};
use crate::error::{corrupt, Result};
use crate::write::{put_u32, put_u64, seal};

const NODE_HEADER: usize = 56;
const TOC_INCREMENT: usize = 8;
const BTOFF_INVALID: u16 = 0xffff;
/// `BTREE_KV_NONALIGNED`
pub const BTREE_KV_NONALIGNED: u32 = 0x40;

/// One entry: a key, and a value unless it is a ghost.
pub type Entry = (Vec<u8>, Option<Vec<u8>>);

pub struct NodeBuild<'a> {
    pub oid: u64,
    pub xid: u64,
    /// Object type including storage flags, e.g. `OBJ_PHYSICAL | OBJECT_TYPE_BTREE`.
    pub obj_type: u32,
    pub subtype: u32,
    pub level: u16,
    pub root: bool,
    /// Key and value sizes when the tree's keys and values are fixed-size.
    pub fixed: Option<(usize, usize)>,
    /// Tree-wide info; the root node carries it.
    pub info: BTreeInfo,
    pub entries: &'a [Entry],
}

fn align8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

/// Space for table of contents, keys and values in a node.
fn data_space(node_size: usize, root: bool) -> usize {
    node_size - NODE_HEADER - if root { BTREE_INFO_SIZE } else { 0 }
}

/// How many table-of-contents slots a node reserves. Fixed-size nodes reserve
/// room for the most entries that could ever fit (rounded up to the TOC
/// increment), as Apple's implementation does; variable-size nodes grow in
/// increments and start at two.
fn toc_slots(b: &NodeBuild, node_size: usize, leaf: bool) -> usize {
    match b.fixed {
        Some((k, v)) => {
            let v = if leaf { v } else { 8 };
            let max = data_space(node_size, b.root) / (4 + k + v);
            max.div_ceil(TOC_INCREMENT) * TOC_INCREMENT
        }
        None => (b.entries.len().div_ceil(TOC_INCREMENT) * TOC_INCREMENT).max(2 * TOC_INCREMENT),
    }
}

/// Whether `entries` fit in one node of this kind.
pub fn fits(b: &NodeBuild, node_size: usize) -> bool {
    layout(b, node_size).is_ok()
}

fn layout(b: &NodeBuild, node_size: usize) -> Result<(usize, usize, usize)> {
    let leaf = b.level == 0;
    let aligned = b.info.flags & BTREE_KV_NONALIGNED == 0;
    let pad = |n: usize| if aligned { align8(n) } else { n };
    let toc = toc_slots(b, node_size, leaf) * if b.fixed.is_some() { 4 } else { 8 };
    let keys: usize = b.entries.iter().map(|(k, _)| pad(k.len())).sum();
    let vals: usize = b.entries.iter().map(|(_, v)| v.as_ref().map_or(0, |v| pad(v.len()))).sum();
    if b.entries.len() * if b.fixed.is_some() { 4 } else { 8 } > toc || toc + keys + vals > data_space(node_size, b.root) {
        return Err(corrupt("B-tree node overflow"));
    }
    Ok((toc, keys, vals))
}

/// Builds and checksums one node.
pub fn build(b: &NodeBuild, node_size: usize) -> Result<Vec<u8>> {
    let leaf = b.level == 0;
    let aligned = b.info.flags & BTREE_KV_NONALIGNED == 0;
    let pad = |n: usize| if aligned { align8(n) } else { n };
    let (toc, keys_len, vals_len) = layout(b, node_size)?;
    let space = data_space(node_size, b.root);
    let mut n = vec![0u8; node_size];
    put_u64(&mut n, 8, b.oid);
    put_u64(&mut n, 16, b.xid);
    put_u32(&mut n, 24, b.obj_type);
    put_u32(&mut n, 28, b.subtype);
    let mut flags = 0u16;
    if b.root {
        flags |= BTNODE_ROOT;
    }
    if leaf {
        flags |= BTNODE_LEAF;
    }
    // Index nodes hold fixed-size child ids, so they qualify whenever keys are fixed.
    if b.fixed.is_some() {
        flags |= BTNODE_FIXED_KV_SIZE;
    }
    n[32..34].copy_from_slice(&flags.to_le_bytes());
    n[34..36].copy_from_slice(&b.level.to_le_bytes());
    put_u32(&mut n, 36, b.entries.len() as u32);
    let put16 = |n: &mut [u8], off: usize, v: u16| n[off..off + 2].copy_from_slice(&v.to_le_bytes());
    put16(&mut n, 40, 0); // table space: offset
    put16(&mut n, 42, toc as u16); // table space: length
    put16(&mut n, 44, keys_len as u16); // free space: offset from the key area
    put16(&mut n, 46, (space - toc - keys_len - vals_len) as u16);
    put16(&mut n, 48, BTOFF_INVALID); // key free list
    put16(&mut n, 50, 0);
    put16(&mut n, 52, BTOFF_INVALID); // value free list
    put16(&mut n, 54, 0);

    let key_area = NODE_HEADER + toc;
    let val_end = NODE_HEADER + space;
    let (mut koff, mut voff) = (0usize, 0usize);
    for (i, (k, v)) in b.entries.iter().enumerate() {
        n[key_area + koff..key_area + koff + k.len()].copy_from_slice(k);
        let v_at = match v {
            Some(v) => {
                voff += pad(v.len());
                n[val_end - voff..val_end - voff + v.len()].copy_from_slice(v);
                voff as u16
            }
            None => BTOFF_INVALID,
        };
        if b.fixed.is_some() {
            let e = NODE_HEADER + i * 4;
            put16(&mut n, e, koff as u16);
            put16(&mut n, e + 2, v_at);
        } else {
            let e = NODE_HEADER + i * 8;
            put16(&mut n, e, koff as u16);
            put16(&mut n, e + 2, k.len() as u16);
            put16(&mut n, e + 4, v_at);
            put16(&mut n, e + 6, v.as_ref().map_or(0, |v| v.len()) as u16);
        }
        koff += pad(k.len());
    }
    if b.root {
        let i = node_size - BTREE_INFO_SIZE;
        put_u32(&mut n, i, b.info.flags);
        put_u32(&mut n, i + 4, b.info.node_size);
        put_u32(&mut n, i + 8, b.info.key_size);
        put_u32(&mut n, i + 12, b.info.val_size);
        put_u32(&mut n, i + 16, b.info.longest_key);
        put_u32(&mut n, i + 20, b.info.longest_val);
        put_u64(&mut n, i + 24, b.info.key_count);
        put_u64(&mut n, i + 32, b.info.node_count);
    }
    seal(&mut n);
    Ok(n)
}
