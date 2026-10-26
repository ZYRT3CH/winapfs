//! Copy-on-write editing of one B-tree within a transaction.
//!
//! The nodes on the paths that are touched are loaded into memory and edited
//! there; [`Tree::flush`] then writes every changed node to a newly allocated
//! block, children first, and reports what moved. Physical trees link
//! children by address, so a moved child changes its parent, up to the root;
//! virtual trees link by object id, and moved nodes are re-pointed in an
//! object map instead. The root of a virtual tree keeps its object id.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::btree::{BTreeInfo, Node};
use crate::btree_write::{build, fits, Entry, NodeBuild};
use crate::bytes::u64_at;
use crate::error::{corrupt, unsupported, Result};
use crate::nx::Container;
use crate::obj::{OBJECT_TYPE_BTREE, OBJECT_TYPE_BTREE_NODE, OBJ_PHYSICAL, OBJ_STORAGETYPE_MASK};
use crate::write::space::Space;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Storage {
    Physical,
    Virtual,
}

#[derive(Clone, Copy)]
enum Child {
    /// Not loaded: a block address (physical trees) or object id (virtual).
    Disk(u64),
    Mem(usize),
}

enum Val {
    Leaf(Option<Vec<u8>>),
    Child(Child),
}

struct MemNode {
    level: u16,
    keys: Vec<Vec<u8>>,
    vals: Vec<Val>,
    /// Virtual object id, once known.
    oid: Option<u64>,
    /// Where the node was read from; freed when it is rewritten.
    paddr: Option<u64>,
    dirty: bool,
    /// Removed from the tree; its block must be freed.
    dead: bool,
}

/// Reads committed nodes: by address for physical trees, through an object
/// map for virtual ones.
pub trait Reader {
    /// Returns the node and the block it was read from.
    fn read(&self, r: u64) -> Result<(Arc<Node>, u64)>;
}

/// What [`Tree::flush`] did.
pub struct Flushed {
    /// The root: its address (physical) or object id (virtual).
    pub root: u64,
    /// Virtual trees: nodes written, as (oid, new address).
    pub moved: Vec<(u64, u64)>,
    /// Virtual trees: object ids of nodes no longer in the tree.
    pub removed: Vec<u64>,
    pub allocated: u64,
    pub freed: u64,
}

pub struct Tree {
    storage: Storage,
    subtype: u32,
    info: BTreeInfo,
    fixed: Option<(usize, usize)>,
    cmp: fn(&[u8], &[u8]) -> Ordering,
    nodes: Vec<MemNode>,
    root: usize,
    node_size: usize,
    changed: bool,
}

type Path = Vec<(usize, usize)>;

impl Tree {
    /// Opens the tree whose root is `root` (an address or an object id).
    pub fn open(rd: &dyn Reader, root: u64, cmp: fn(&[u8], &[u8]) -> Ordering) -> Result<Self> {
        let (node, paddr) = rd.read(root)?;
        let info = node.info()?;
        let storage =
            if node.hdr.obj_type & OBJ_STORAGETYPE_MASK == OBJ_PHYSICAL { Storage::Physical } else { Storage::Virtual };
        let fixed = (info.key_size != 0).then_some((info.key_size as usize, info.val_size as usize));
        let mut t = Self {
            storage,
            subtype: node.hdr.subtype,
            info,
            fixed,
            cmp,
            nodes: Vec::new(),
            root: 0,
            node_size: node.data.len(),
            changed: false,
        };
        t.root = t.adopt(&node, paddr)?;
        Ok(t)
    }

    fn adopt(&mut self, node: &Node, paddr: u64) -> Result<usize> {
        let entries = node.all_entries(&self.info)?;
        let mut keys = Vec::with_capacity(entries.len());
        let mut vals = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            keys.push(k);
            vals.push(if node.is_leaf() {
                Val::Leaf(v)
            } else {
                Val::Child(Child::Disk(u64_at(v.as_deref().ok_or_else(|| corrupt("index entry without child"))?, 0)?))
            });
        }
        let oid = (self.storage == Storage::Virtual).then_some(node.hdr.oid);
        self.nodes.push(MemNode { level: node.level, keys, vals, oid, paddr: Some(paddr), dirty: false, dead: false });
        Ok(self.nodes.len() - 1)
    }

    fn child(&mut self, rd: &dyn Reader, n: usize, i: usize) -> Result<usize> {
        match self.nodes[n].vals[i] {
            Val::Child(Child::Mem(m)) => Ok(m),
            Val::Child(Child::Disk(r)) => {
                let (node, paddr) = rd.read(r)?;
                if node.level + 1 != self.nodes[n].level {
                    return Err(corrupt("B-tree child at the wrong level"));
                }
                let m = self.adopt(&node, paddr)?;
                self.nodes[n].vals[i] = Val::Child(Child::Mem(m));
                Ok(m)
            }
            Val::Leaf(_) => Err(corrupt("descending into a leaf")),
        }
    }

    /// Path from the root to the leaf where `key` belongs, as (node, index of
    /// the child taken) pairs; the leaf's index is unused.
    fn path(&mut self, rd: &dyn Reader, key: &[u8]) -> Result<Path> {
        let mut path = Vec::new();
        let mut n = self.root;
        while self.nodes[n].level > 0 {
            let i = self.nodes[n].keys.partition_point(|k| (self.cmp)(k, key) != Ordering::Greater).saturating_sub(1);
            path.push((n, i));
            n = self.child(rd, n, i)?;
        }
        path.push((n, 0));
        Ok(path)
    }

    /// The value stored under `key`: `Some(None)` for a ghost entry.
    pub fn get(&mut self, rd: &dyn Reader, key: &[u8]) -> Result<Option<Option<Vec<u8>>>> {
        let leaf = self.path(rd, key)?.last().unwrap().0;
        let node = &self.nodes[leaf];
        Ok(node.keys.binary_search_by(|k| (self.cmp)(k, key)).ok().map(|i| match &node.vals[i] {
            Val::Leaf(v) => v.clone(),
            Val::Child(_) => None,
        }))
    }

    /// The entry with the greatest key `<= key`.
    pub fn floor(&mut self, rd: &dyn Reader, key: &[u8]) -> Result<Option<Entry>> {
        let path = self.path(rd, key)?;
        let leaf = path.last().unwrap().0;
        let i = self.nodes[leaf].keys.partition_point(|k| (self.cmp)(k, key) != Ordering::Greater);
        if i > 0 {
            return Ok(Some(self.leaf_entry(leaf, i - 1)));
        }
        // An index key can be lower than its child's first key (after that
        // key was removed); then the answer is the last entry to the left.
        for &(n, ci) in path[..path.len() - 1].iter().rev() {
            if ci > 0 && (self.cmp)(&self.nodes[n].keys[ci], key) != Ordering::Greater {
                let mut m = self.child(rd, n, ci - 1)?;
                while self.nodes[m].level > 0 {
                    let last = self.nodes[m].keys.len() - 1;
                    m = self.child(rd, m, last)?;
                }
                let len = self.nodes[m].keys.len();
                return Ok((len > 0).then(|| self.leaf_entry(m, len - 1)));
            }
        }
        Ok(None)
    }

    fn leaf_entry(&self, n: usize, i: usize) -> Entry {
        let v = match &self.nodes[n].vals[i] {
            Val::Leaf(v) => v.clone(),
            Val::Child(_) => None,
        };
        (self.nodes[n].keys[i].clone(), v)
    }

    /// Every leaf entry with `lo <= key < hi`, in order.
    pub fn range(&mut self, rd: &dyn Reader, lo: &[u8], hi: &[u8]) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        self.collect(rd, self.root, lo, hi, &mut out)?;
        Ok(out)
    }

    fn collect(&mut self, rd: &dyn Reader, n: usize, lo: &[u8], hi: &[u8], out: &mut Vec<Entry>) -> Result<()> {
        let len = self.nodes[n].keys.len();
        if self.nodes[n].level == 0 {
            for i in 0..len {
                let k = &self.nodes[n].keys[i];
                if (self.cmp)(k, lo) != Ordering::Less && (self.cmp)(k, hi) == Ordering::Less {
                    if let Val::Leaf(v) = &self.nodes[n].vals[i] {
                        out.push((k.clone(), v.clone()));
                    }
                }
            }
            return Ok(());
        }
        for i in 0..len {
            if (self.cmp)(&self.nodes[n].keys[i], hi) != Ordering::Less {
                break;
            }
            // Child i holds keys in [key i, key i+1).
            if i + 1 < len && (self.cmp)(&self.nodes[n].keys[i + 1], lo) != Ordering::Greater {
                continue;
            }
            let c = self.child(rd, n, i)?;
            self.collect(rd, c, lo, hi, out)?;
        }
        Ok(())
    }

    /// Inserts or replaces `key`.
    pub fn upsert(&mut self, rd: &dyn Reader, key: Vec<u8>, val: Option<Vec<u8>>) -> Result<()> {
        let path = self.path(rd, &key)?;
        let leaf = path.last().unwrap().0;
        if let Some(v) = &val {
            self.info.longest_val = self.info.longest_val.max(v.len() as u32);
        }
        let cmp = self.cmp;
        let node = &mut self.nodes[leaf];
        match node.keys.binary_search_by(|k| cmp(k, &key)) {
            Ok(i) => node.vals[i] = Val::Leaf(val),
            Err(i) => {
                self.info.longest_key = self.info.longest_key.max(key.len() as u32);
                node.keys.insert(i, key);
                node.vals.insert(i, Val::Leaf(val));
                self.info.key_count += 1;
            }
        }
        self.touch(&path);
        self.split_up(&path)
    }

    /// Removes `key`; returns whether it was there.
    pub fn remove(&mut self, rd: &dyn Reader, key: &[u8]) -> Result<bool> {
        let path = self.path(rd, key)?;
        let leaf = path.last().unwrap().0;
        let Ok(i) = self.nodes[leaf].keys.binary_search_by(|k| (self.cmp)(k, key)) else { return Ok(false) };
        self.nodes[leaf].keys.remove(i);
        self.nodes[leaf].vals.remove(i);
        self.info.key_count -= 1;
        self.touch(&path);
        // Drop nodes that became empty (never the root itself).
        for depth in (1..path.len()).rev() {
            let (n, _) = path[depth];
            if !self.nodes[n].keys.is_empty() {
                break;
            }
            self.nodes[n].dead = true;
            self.info.node_count -= 1;
            let (parent, ci) = path[depth - 1];
            self.nodes[parent].keys.remove(ci);
            self.nodes[parent].vals.remove(ci);
            self.nodes[parent].dirty = true;
        }
        let root = self.root;
        if self.nodes[root].keys.is_empty() {
            // Everything is gone: the root becomes an empty leaf.
            self.nodes[root].level = 0;
        }
        // A root index node left with one child hands the root role down.
        while self.nodes[self.root].level > 0 && self.nodes[self.root].keys.len() == 1 {
            let old = self.root;
            let only = self.child(rd, old, 0)?;
            // The root's object id stays the tree's root id; the dead node
            // takes the child's old id, which is reported as removed.
            let (a, b) = (self.nodes[old].oid, self.nodes[only].oid);
            self.nodes[old].oid = b;
            self.nodes[only].oid = a;
            self.nodes[old].dead = true;
            self.info.node_count -= 1;
            self.nodes[only].dirty = true;
            self.root = only;
        }
        self.fix_keys(&path);
        Ok(true)
    }

    /// Marks what a change on `path` forces to be rewritten: the leaf, and the
    /// root (it carries the key count). Physical trees rewrite the whole
    /// path, since a moved child changes its parent.
    fn touch(&mut self, path: &[(usize, usize)]) {
        self.changed = true;
        let root = self.root;
        self.nodes[root].dirty = true;
        self.nodes[path.last().unwrap().0].dirty = true;
        if self.storage == Storage::Physical {
            for &(n, _) in path {
                self.nodes[n].dirty = true;
            }
        }
        self.fix_keys(path);
    }

    /// Keeps each index key equal to the first key of its child.
    fn fix_keys(&mut self, path: &[(usize, usize)]) {
        for depth in (1..path.len()).rev() {
            let (child, _) = path[depth];
            let (parent, ci) = path[depth - 1];
            if self.nodes[child].dead || self.nodes[parent].dead || self.nodes[child].keys.is_empty() {
                continue;
            }
            if ci < self.nodes[parent].keys.len() && self.nodes[parent].keys[ci] != self.nodes[child].keys[0] {
                self.nodes[parent].keys[ci] = self.nodes[child].keys[0].clone();
                self.nodes[parent].dirty = true;
            }
        }
    }

    fn entries_of(&self, n: usize) -> Vec<Entry> {
        let node = &self.nodes[n];
        node.keys
            .iter()
            .zip(&node.vals)
            .map(|(k, v)| match v {
                Val::Leaf(v) => (k.clone(), v.clone()),
                // Only the size matters when checking whether entries fit.
                Val::Child(_) => (k.clone(), Some(vec![0u8; 8])),
            })
            .collect()
    }

    fn fits_as(&self, root: bool, level: u16, entries: &[Entry]) -> bool {
        fits(&self.spec(root, level, entries, 0, 0), self.node_size)
    }

    fn spec<'a>(&self, root: bool, level: u16, entries: &'a [Entry], oid: u64, xid: u64) -> NodeBuild<'a> {
        let storage = if self.storage == Storage::Physical { OBJ_PHYSICAL } else { 0 };
        NodeBuild {
            oid,
            xid,
            obj_type: storage | if root { OBJECT_TYPE_BTREE } else { OBJECT_TYPE_BTREE_NODE },
            subtype: self.subtype,
            level,
            root,
            fixed: self.fixed,
            info: self.info,
            entries,
        }
    }

    /// Splits overfull nodes on `path`, bottom up; grows a new root if needed.
    fn split_up(&mut self, path: &[(usize, usize)]) -> Result<()> {
        for depth in (0..path.len()).rev() {
            let (n, _) = path[depth];
            let level = self.nodes[n].level;
            let all = self.entries_of(n);
            if self.fits_as(n == self.root, level, &all) {
                continue;
            }
            let len = all.len();
            if len < 2 {
                return Err(unsupported("a B-tree entry too large for a node"));
            }
            // Cut near the middle by size, then adjust until both halves fit.
            let size = |e: &Entry| e.0.len() + e.1.as_ref().map_or(0, Vec::len) + 8;
            let total: usize = all.iter().map(size).sum();
            let (mut cut, mut acc) = (0, 0);
            while cut < len - 1 && acc + size(&all[cut]) <= total / 2 {
                acc += size(&all[cut]);
                cut += 1;
            }
            let mut cut = cut.max(1);
            while cut > 1 && !self.fits_as(false, level, &all[..cut]) {
                cut -= 1;
            }
            while cut < len - 1 && !self.fits_as(false, level, &all[cut..]) && self.fits_as(false, level, &all[..cut + 1]) {
                cut += 1;
            }
            if !self.fits_as(false, level, &all[..cut]) || !self.fits_as(false, level, &all[cut..]) {
                return Err(unsupported("B-tree entries too large to split"));
            }
            let right_keys = self.nodes[n].keys.split_off(cut);
            let right_vals = self.nodes[n].vals.split_off(cut);
            let first = right_keys[0].clone();
            self.nodes.push(MemNode {
                level,
                keys: right_keys,
                vals: right_vals,
                oid: None,
                paddr: None,
                dirty: true,
                dead: false,
            });
            let right = self.nodes.len() - 1;
            self.info.node_count += 1;
            if depth == 0 {
                // The root split: a new root above both halves takes over the
                // old root's object id and block; the old root becomes an
                // ordinary node with fresh ones.
                let left_first = self.nodes[n].keys[0].clone();
                let oid = self.nodes[n].oid.take();
                let paddr = self.nodes[n].paddr.take();
                self.nodes.push(MemNode {
                    level: level + 1,
                    keys: vec![left_first, first],
                    vals: vec![Val::Child(Child::Mem(n)), Val::Child(Child::Mem(right))],
                    oid,
                    paddr,
                    dirty: true,
                    dead: false,
                });
                self.info.node_count += 1;
                self.root = self.nodes.len() - 1;
            } else {
                let (parent, ci) = path[depth - 1];
                self.nodes[parent].keys.insert(ci + 1, first);
                self.nodes[parent].vals.insert(ci + 1, Val::Child(Child::Mem(right)));
                self.nodes[parent].dirty = true;
            }
        }
        Ok(())
    }

    pub fn changed(&self) -> bool {
        self.changed
    }

    /// Writes every changed node to a new block. `next_oid` supplies object
    /// ids for new nodes of virtual trees.
    pub fn flush(
        mut self,
        c: &Container,
        space: &mut Space,
        writes: &mut Vec<(u64, Vec<u8>)>,
        xid: u64,
        next_oid: &mut u64,
    ) -> Result<Flushed> {
        let mut out = Flushed { root: 0, moved: Vec::new(), removed: Vec::new(), allocated: 0, freed: 0 };
        for node in &mut self.nodes {
            if node.dead {
                if let Some(p) = node.paddr.take() {
                    space.free(p);
                    out.freed += 1;
                }
                if let Some(oid) = node.oid {
                    out.removed.push(oid);
                }
            }
        }
        let root = self.root;
        let mut w = Writes { c, space, writes, xid, next_oid, out: &mut out };
        let r = self.write(&mut w, root)?;
        out.root = r;
        Ok(out)
    }

    fn write(&mut self, w: &mut Writes, n: usize) -> Result<u64> {
        // Children first, so their new addresses or ids are known.
        for i in 0..self.nodes[n].vals.len() {
            if let Val::Child(Child::Mem(m)) = self.nodes[n].vals[i] {
                let r = self.write(w, m)?;
                self.nodes[n].vals[i] = Val::Child(Child::Disk(r));
            }
        }
        if !self.nodes[n].dirty {
            return Ok(match self.storage {
                Storage::Physical => self.nodes[n].paddr.unwrap(),
                Storage::Virtual => self.nodes[n].oid.unwrap(),
            });
        }
        let node = &self.nodes[n];
        let entries: Vec<Entry> = node
            .keys
            .iter()
            .zip(&node.vals)
            .map(|(k, v)| match v {
                Val::Leaf(v) => (k.clone(), v.clone()),
                Val::Child(Child::Disk(r)) => (k.clone(), Some(r.to_le_bytes().to_vec())),
                Val::Child(Child::Mem(_)) => unreachable!("children are written first"),
            })
            .collect();
        let paddr = w.space.alloc(w.c)?;
        w.out.allocated += 1;
        if let Some(old) = self.nodes[n].paddr.replace(paddr) {
            w.space.free(old);
            w.out.freed += 1;
        }
        let oid = match self.storage {
            Storage::Physical => paddr,
            Storage::Virtual => *self.nodes[n].oid.get_or_insert_with(|| {
                let o = *w.next_oid;
                *w.next_oid += 1;
                o
            }),
        };
        let nb = self.spec(n == self.root, self.nodes[n].level, &entries, oid, w.xid);
        w.writes.push((paddr, build(&nb, self.node_size)?));
        if self.storage == Storage::Virtual {
            w.out.moved.push((oid, paddr));
        }
        Ok(oid)
    }
}

struct Writes<'a> {
    c: &'a Container,
    space: &'a mut Space,
    writes: &'a mut Vec<(u64, Vec<u8>)>,
    xid: u64,
    next_oid: &'a mut u64,
    out: &'a mut Flushed,
}

/// Reads a physical tree's nodes by address.
pub struct PhysReader<'a>(pub &'a Container);

impl Reader for PhysReader<'_> {
    fn read(&self, paddr: u64) -> Result<(Arc<Node>, u64)> {
        Ok((self.0.node(paddr)?, paddr))
    }
}

/// Object-map key order: object id, then transaction id.
pub fn omap_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let k = |x: &[u8]| (u64::from_le_bytes(x[0..8].try_into().unwrap()), u64::from_le_bytes(x[8..16].try_into().unwrap()));
    k(a).cmp(&k(b))
}

fn omap_key(oid: u64, xid: u64) -> Vec<u8> {
    let mut k = oid.to_le_bytes().to_vec();
    k.extend(xid.to_le_bytes());
    k
}

/// Points `oid` at `paddr` as of `xid`, dropping its older versions (there
/// are no snapshots to keep them for).
pub fn omap_put(t: &mut Tree, rd: &dyn Reader, oid: u64, xid: u64, paddr: u64, size: u32) -> Result<()> {
    omap_drop(t, rd, oid)?;
    let mut v = 0u32.to_le_bytes().to_vec(); // flags
    v.extend(size.to_le_bytes());
    v.extend(paddr.to_le_bytes());
    t.upsert(rd, omap_key(oid, xid), Some(v))
}

/// Removes every version of `oid`.
pub fn omap_drop(t: &mut Tree, rd: &dyn Reader, oid: u64) -> Result<()> {
    for (k, _) in t.range(rd, &omap_key(oid, 0), &omap_key(oid + 1, 0))? {
        t.remove(rd, &k)?;
    }
    Ok(())
}
