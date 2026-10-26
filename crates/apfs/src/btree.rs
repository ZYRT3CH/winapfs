//! B-tree nodes (`btree_node_phys_t`) and read-only traversal.
//!
//! APFS B-trees have no sibling links, so range scans walk down from the root
//! and skip children whose key range cannot overlap the window.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::bytes::{slice_at, u16_at, u32_at, u64_at};
use crate::error::{corrupt, unsupported, Result};
use crate::obj::{ObjHeader, OBJECT_TYPE_BTREE, OBJECT_TYPE_BTREE_NODE};

pub const BTNODE_ROOT: u16 = 0x0001;
pub const BTNODE_LEAF: u16 = 0x0002;
pub const BTNODE_FIXED_KV_SIZE: u16 = 0x0004;
pub const BTNODE_HASHED: u16 = 0x0008;

pub const BTREE_PHYSICAL: u32 = 0x0010;
pub const BTREE_HASHED: u32 = 0x0080;

const NODE_DATA: usize = 56;
pub const BTREE_INFO_SIZE: usize = 40;
const BTOFF_INVALID: u16 = 0xffff;
const MAX_DEPTH: u16 = 32;

#[derive(Debug, Clone, Copy)]
pub struct BTreeInfo {
    pub flags: u32,
    pub node_size: u32,
    pub key_size: u32,
    pub val_size: u32,
    pub longest_key: u32,
    pub longest_val: u32,
    pub key_count: u64,
    pub node_count: u64,
}

pub struct Node {
    pub data: Vec<u8>,
    pub hdr: ObjHeader,
    pub flags: u16,
    pub level: u16,
    pub nkeys: u32,
    toc_start: usize,
    key_start: usize,
    val_end: usize,
}

impl Node {
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        let hdr = ObjHeader::parse(&data)?;
        if !matches!(hdr.kind(), OBJECT_TYPE_BTREE | OBJECT_TYPE_BTREE_NODE) {
            return Err(corrupt(format!("object {:#x} is type {:#x}, expected a B-tree node", hdr.oid, hdr.kind())));
        }
        let flags = u16_at(&data, 32)?;
        let level = u16_at(&data, 34)?;
        let nkeys = u32_at(&data, 36)?;
        let toc_off = u16_at(&data, 40)? as usize;
        let toc_len = u16_at(&data, 42)? as usize;
        if flags & BTNODE_HASHED != 0 {
            return Err(unsupported("hashed B-tree nodes (sealed system volumes)"));
        }
        let toc_start = NODE_DATA + toc_off;
        let key_start = toc_start + toc_len;
        let val_end = if flags & BTNODE_ROOT != 0 { data.len() - BTREE_INFO_SIZE } else { data.len() };
        if key_start > val_end || level > MAX_DEPTH {
            return Err(corrupt(format!("B-tree node {:#x} has an impossible layout", hdr.oid)));
        }
        Ok(Self { data, hdr, flags, level, nkeys, toc_start, key_start, val_end })
    }

    pub fn is_leaf(&self) -> bool {
        self.flags & BTNODE_LEAF != 0
    }

    pub fn is_root(&self) -> bool {
        self.flags & BTNODE_ROOT != 0
    }

    /// The tree-wide info stored at the end of the root node.
    pub fn info(&self) -> Result<BTreeInfo> {
        if !self.is_root() {
            return Err(corrupt("btree_info_t requested from a non-root node"));
        }
        let b = &self.data[self.data.len() - BTREE_INFO_SIZE..];
        Ok(BTreeInfo {
            flags: u32_at(b, 0)?,
            node_size: u32_at(b, 4)?,
            key_size: u32_at(b, 8)?,
            val_size: u32_at(b, 12)?,
            longest_key: u32_at(b, 16)?,
            longest_val: u32_at(b, 20)?,
            key_count: u64_at(b, 24)?,
            node_count: u64_at(b, 32)?,
        })
    }

    /// Key and value of entry `i`. The value is `None` for a ghost entry.
    pub fn entry(&self, i: usize, info: &BTreeInfo) -> Result<(&[u8], Option<&[u8]>)> {
        if i >= self.nkeys as usize {
            return Err(corrupt(format!("entry {i} of a {}-key node", self.nkeys)));
        }
        let (k_off, k_len, v_off, v_len) = if self.flags & BTNODE_FIXED_KV_SIZE != 0 {
            let e = self.toc_start + i * 4;
            let v_len = if self.is_leaf() { info.val_size as usize } else { 8 };
            (u16_at(&self.data, e)?, info.key_size as usize, u16_at(&self.data, e + 2)?, v_len)
        } else {
            let e = self.toc_start + i * 8;
            (
                u16_at(&self.data, e)?,
                u16_at(&self.data, e + 2)? as usize,
                u16_at(&self.data, e + 4)?,
                u16_at(&self.data, e + 6)? as usize,
            )
        };
        let key = slice_at(&self.data, self.key_start + k_off as usize, k_len)?;
        if v_off == BTOFF_INVALID {
            return Ok((key, None));
        }
        let v_start = self
            .val_end
            .checked_sub(v_off as usize)
            .ok_or_else(|| corrupt(format!("value offset {v_off} outside node {:#x}", self.hdr.oid)))?;
        Ok((key, Some(slice_at(&self.data, v_start, v_len)?)))
    }

    /// All entries, ghosts included, as owned key/value pairs.
    pub fn all_entries(&self, info: &BTreeInfo) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
        (0..self.nkeys as usize)
            .map(|i| self.entry(i, info).map(|(k, v)| (k.to_vec(), v.map(<[u8]>::to_vec))))
            .collect()
    }

    pub fn is_fixed(&self) -> bool {
        self.flags & BTNODE_FIXED_KV_SIZE != 0
    }

    fn child(&self, i: usize, info: &BTreeInfo) -> Result<u64> {
        let (_, v) = self.entry(i, info)?;
        let v = v.ok_or_else(|| corrupt(format!("index node {:#x} entry {i} has no child", self.hdr.oid)))?;
        u64_at(v, 0)
    }
}

pub trait NodeSource {
    /// Reads and checksum-verifies the node at physical address `paddr`.
    fn read_node(&self, paddr: u64) -> Result<Arc<Node>>;
}

/// Maps a child pointer to a physical address: the identity for physical
/// trees, an object-map lookup for virtual ones.
pub type Resolve<'a> = &'a dyn Fn(u64) -> Result<u64>;

/// Calls `f(key, value)` for every live leaf entry inside the window, in key
/// order. `window(key)` says whether a key is below (`Less`), inside
/// (`Equal`) or above (`Greater`) the wanted range. `f` returns `false` to stop.
pub fn scan(
    src: &dyn NodeSource,
    root: &Node,
    info: &BTreeInfo,
    resolve: Resolve,
    window: &dyn Fn(&[u8]) -> Result<Ordering>,
    f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>,
) -> Result<()> {
    let mut live = |k: &[u8], v: Option<&[u8]>| match v {
        Some(v) => f(k, v),
        None => Ok(true),
    };
    scan_node(src, root, info, resolve, window, &mut live).map(|_| ())
}

/// [`scan`] including ghost entries (keys without a value), which some
/// trees use deliberately: in a space-manager free queue a ghost means a
/// count of one.
pub fn scan_with_ghosts(
    src: &dyn NodeSource,
    root: &Node,
    info: &BTreeInfo,
    resolve: Resolve,
    f: &mut dyn FnMut(&[u8], Option<&[u8]>) -> Result<bool>,
) -> Result<()> {
    scan_node(src, root, info, resolve, &|_| Ok(Ordering::Equal), f).map(|_| ())
}

fn scan_node(
    src: &dyn NodeSource,
    node: &Node,
    info: &BTreeInfo,
    resolve: Resolve,
    window: &dyn Fn(&[u8]) -> Result<Ordering>,
    f: &mut dyn FnMut(&[u8], Option<&[u8]>) -> Result<bool>,
) -> Result<bool> {
    let n = node.nkeys as usize;
    if node.is_leaf() {
        for i in 0..n {
            let (k, v) = node.entry(i, info)?;
            match window(k)? {
                Ordering::Less => {}
                Ordering::Greater => return Ok(false),
                Ordering::Equal => {
                    if !f(k, v)? {
                        return Ok(false);
                    }
                }
            }
        }
        return Ok(true);
    }
    for i in 0..n {
        let (k, _) = node.entry(i, info)?;
        if window(k)? == Ordering::Greater {
            return Ok(false);
        }
        // Child i holds keys in [key_i, key_i+1); skip it if that range ends below the window.
        if i + 1 < n && window(node.entry(i + 1, info)?.0)? == Ordering::Less {
            continue;
        }
        let child = read_child(src, node, i, info, resolve)?;
        if !scan_node(src, &child, info, resolve, window, f)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Finds the greatest entry whose key is `<=` the target. `cmp(key)` orders
/// a key relative to the target.
pub fn lookup_le(
    src: &dyn NodeSource,
    root: &Node,
    info: &BTreeInfo,
    resolve: Resolve,
    cmp: &dyn Fn(&[u8]) -> Result<Ordering>,
) -> Result<Option<(Vec<u8>, Option<Vec<u8>>)>> {
    let mut owned: Option<Arc<Node>> = None;
    loop {
        let node: &Node = owned.as_deref().unwrap_or(root);
        // Binary search for the first key greater than the target.
        let (mut lo, mut hi) = (0usize, node.nkeys as usize);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cmp(node.entry(mid, info)?.0)? == Ordering::Greater {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        if lo == 0 {
            return Ok(None);
        }
        if node.is_leaf() {
            let (k, v) = node.entry(lo - 1, info)?;
            return Ok(Some((k.to_vec(), v.map(<[u8]>::to_vec))));
        }
        owned = Some(read_child(src, node, lo - 1, info, resolve)?);
    }
}

fn read_child(src: &dyn NodeSource, node: &Node, i: usize, info: &BTreeInfo, resolve: Resolve) -> Result<Arc<Node>> {
    let child = src.read_node(resolve(node.child(i, info)?)?)?;
    if child.level + 1 != node.level {
        return Err(corrupt(format!(
            "B-tree node {:#x} at level {} has a child at level {}",
            node.hdr.oid, node.level, child.level
        )));
    }
    Ok(child)
}
