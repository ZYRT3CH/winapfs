//! Block allocation for one transaction.
//!
//! Main-device blocks are tracked by per-chunk bitmaps; the bitmaps and the
//! chunk-info blocks (CIBs) that point to them live in the space manager's
//! internal pool, which has its own bitmap, kept in a small ring of blocks.
//! Nothing that the previous checkpoint uses is overwritten:
//!
//! - a freed block goes into a free queue and keeps its bit until the next
//!   transaction, so the previous checkpoint stays intact as a fallback;
//! - changed bitmaps and CIBs are written to new internal-pool blocks;
//! - the changed internal-pool bitmap is written to the next ring block.

use std::collections::{BTreeSet, HashMap};

use crate::btree::{self, BTreeInfo, NodeSource};
use crate::btree_write::{build, Entry, NodeBuild};
use crate::bytes::{u32_at, u64_at};
use crate::checksum;
use crate::error::{corrupt, unsupported, Error, Result};
use crate::nx::Container;
use crate::spaceman::{Spaceman, SD_MAIN, SD_TIER2, SFQ_IP, SFQ_MAIN};
use crate::write::{put_u32, put_u64, seal, set_xid};

const NO_INDEX: u16 = 0xffff;
/// A free stretch this long (8 MiB at 4 KiB blocks) is worth seeking out
/// for file data rather than filling smaller gaps.
const BIG_RUN: u64 = 2048;

/// A free-queue entry: `count` blocks from `paddr`, freed in transaction `xid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FqEntry {
    pub xid: u64,
    pub paddr: u64,
    pub count: u64,
}

/// An ephemeral object for the next checkpoint.
pub struct EphObject {
    pub oid: u64,
    pub obj_type: u32,
    pub subtype: u32,
    pub data: Vec<u8>,
}

struct Cib {
    paddr: u64,
    raw: Vec<u8>,
    dirty: bool,
}

pub struct Space {
    xid: u64,
    bs: u64,
    sm: Spaceman,
    sm_raw: Vec<u8>,
    cibs: Vec<Cib>,
    bitmaps: HashMap<usize, Vec<u8>>,
    dirty_chunks: BTreeSet<usize>,
    ip_bitmap: Vec<u8>,
    /// Per bitmap block: changed in this transaction.
    ip_dirty: Vec<bool>,
    fq: [Vec<FqEntry>; 2],
    fq_roots: [Option<(u32, BTreeInfo)>; 2],
    /// Object ids of the free-queue trees' non-root nodes.
    fq_old_nodes: Vec<u64>,
    /// Chunk where the last run was found; earlier ones were full.
    run_hint: usize,
    /// Chunk where the last big run was found; earlier ones have none.
    big_hint: usize,
    /// Chunk bitmaps read from the disk in this transaction.
    pub bitmap_reads: usize,
}

impl Space {
    pub fn load(c: &Container, xid: u64) -> Result<Self> {
        let sm_raw = c.ephemeral(c.sb.spaceman_oid)?;
        let sm = Spaceman::parse(&sm_raw)?;
        let bs = c.block_size() as u64;
        if sm.dev[SD_MAIN].cab_count > 0 {
            return Err(unsupported("writing to containers large enough to need cib address blocks"));
        }
        if sm.dev[SD_TIER2].block_count > 0 {
            return Err(unsupported("writing to Fusion containers"));
        }
        if sm.ip_bm_size_in_blocks == 0 || sm.ip_block_count > sm.ip_bm_size_in_blocks as u64 * bs * 8 {
            return Err(corrupt("the internal-pool bitmap is too small for the internal pool"));
        }
        let mut cibs = Vec::new();
        for paddr in sm.dev_addrs(SD_MAIN)? {
            let raw = c.raw_block(paddr)?;
            if !checksum::verify(&raw) {
                return Err(corrupt(format!("checksum mismatch in chunk-info block {paddr:#x}")));
            }
            cibs.push(Cib { paddr, raw, dirty: false });
        }
        // The bitmap's blocks each sit somewhere in a ring of blocks.
        let mut ip_bitmap = Vec::with_capacity(sm.ip_bm_size_in_blocks as usize * bs as usize);
        for index in sm.ip_bitmap_indices()? {
            if index as u32 >= sm.ip_bm_block_count {
                return Err(corrupt(format!("internal-pool bitmap index {index} outside its ring")));
            }
            ip_bitmap.extend(c.raw_block(sm.ip_bm_base + index as u64)?);
        }
        let ip_dirty = vec![false; sm.ip_bm_size_in_blocks as usize];

        let mut fq: [Vec<FqEntry>; 2] = [Vec::new(), Vec::new()];
        let mut fq_roots = [None, None];
        let fq_old_nodes = std::cell::RefCell::new(Vec::new());
        for (slot, q) in [SFQ_IP, SFQ_MAIN].into_iter().enumerate() {
            let tree_oid = sm.fq[q].tree_oid;
            if tree_oid == 0 {
                continue;
            }
            let root_paddr = c
                .ephemeral_paddr(tree_oid)
                .ok_or_else(|| corrupt(format!("free queue tree {tree_oid:#x} not in the checkpoint")))?;
            let root = c.node(root_paddr)?;
            let info = root.info()?;
            fq_roots[slot] = Some((root.hdr.obj_type, info));
            // Non-root nodes: the tree is rebuilt as one node at commit, so
            // their checkpoint entries go.
            let resolve = |oid: u64| {
                fq_old_nodes.borrow_mut().push(oid);
                c.ephemeral_paddr(oid).ok_or_else(|| corrupt(format!("free queue node {oid:#x} missing")))
            };
            btree::scan_with_ghosts(&NodeReader(c), &root, &info, &resolve, &mut |k, v| {
                fq[slot].push(FqEntry {
                    xid: u64_at(k, 0)?,
                    paddr: u64_at(k, 8)?,
                    count: v.map_or(Ok(1), |v| u64_at(v, 0))?,
                });
                Ok(true)
            })?;
        }
        Ok(Self {
            xid,
            bs,
            sm,
            sm_raw,
            cibs,
            bitmaps: HashMap::new(),
            dirty_chunks: BTreeSet::new(),
            ip_bitmap,
            ip_dirty,
            fq,
            fq_roots,
            fq_old_nodes: fq_old_nodes.into_inner(),
            run_hint: 0,
            big_hint: 0,
            bitmap_reads: 0,
        })
    }

    fn per_cib(&self) -> usize {
        self.sm.chunks_per_cib as usize
    }

    fn chunk_count(&self) -> usize {
        self.sm.dev[SD_MAIN].chunk_count as usize
    }

    /// Byte offset of chunk `ch`'s `chunk_info_t` within its CIB.
    fn chunk_pos(&self, ch: usize) -> (usize, usize) {
        (ch / self.per_cib(), 40 + (ch % self.per_cib()) * 32)
    }

    fn chunk_field_u32(&self, ch: usize, off: usize) -> u32 {
        let (cib, e) = self.chunk_pos(ch);
        u32_at(&self.cibs[cib].raw, e + off).unwrap_or(0)
    }

    fn chunk_free(&self, ch: usize) -> u32 {
        self.chunk_field_u32(ch, 20) & crate::spaceman::CI_COUNT_MASK
    }

    fn chunk_blocks(&self, ch: usize) -> u32 {
        self.chunk_field_u32(ch, 16) & crate::spaceman::CI_COUNT_MASK
    }

    fn chunk_bitmap_addr(&self, ch: usize) -> u64 {
        let (cib, e) = self.chunk_pos(ch);
        u64_at(&self.cibs[cib].raw, e + 24).unwrap_or(0)
    }

    /// Adjusts a chunk's free count, keeping the reserved high bits.
    fn add_chunk_free(&mut self, ch: usize, delta: i64) {
        let (cib, e) = self.chunk_pos(ch);
        let word = u32_at(&self.cibs[cib].raw, e + 20).unwrap_or(0);
        let free = ((word & crate::spaceman::CI_COUNT_MASK) as i64 + delta) as u32;
        put_u32(&mut self.cibs[cib].raw, e + 20, (word & !crate::spaceman::CI_COUNT_MASK) | free);
        self.cibs[cib].dirty = true;
        self.dirty_chunks.insert(ch);
        let dev_free = &mut self.sm.dev[SD_MAIN].free_count;
        *dev_free = (*dev_free as i64 + delta) as u64;
    }

    fn bitmap(&mut self, c: &Container, ch: usize) -> Result<&mut Vec<u8>> {
        if !self.bitmaps.contains_key(&ch) {
            let addr = self.chunk_bitmap_addr(ch);
            let bm = if addr == 0 {
                vec![0u8; self.bs as usize]
            } else {
                self.bitmap_reads += 1;
                c.raw_block(addr)?
            };
            self.bitmaps.insert(ch, bm);
        }
        Ok(self.bitmaps.get_mut(&ch).unwrap())
    }

    /// Allocates one main-device block.
    pub fn alloc(&mut self, c: &Container) -> Result<u64> {
        let bpc = self.sm.blocks_per_chunk as u64;
        for ch in 0..self.chunk_count() {
            if self.chunk_free(ch) == 0 {
                continue;
            }
            let blocks = self.chunk_blocks(ch) as usize;
            let bm = self.bitmap(c, ch)?;
            if let Some(i) = (0..blocks).find(|&i| bm[i / 8] >> (i % 8) & 1 == 0) {
                bm[i / 8] |= 1 << (i % 8);
                self.add_chunk_free(ch, -1);
                return Ok(ch as u64 * bpc + i as u64);
            }
        }
        Err(Error::NoSpace("the container is full".into()))
    }

    /// Allocates up to `max` contiguous main-device blocks: the first free
    /// run, cut at `max` or at the end of its chunk. Returns (first block,
    /// count).
    pub fn alloc_run(&mut self, c: &Container, max: u64) -> Result<(u64, u64)> {
        // File data: look for a free stretch big enough not to fragment the
        // file first, and fill small gaps only when there is none.
        let min_run = max.min(BIG_RUN);
        if min_run >= 16 {
            if let Some(run) = self.find_big_run(c, min_run, max)? {
                return Ok(run);
            }
        }
        let bpc = self.sm.blocks_per_chunk as u64;
        let used = |bm: &[u8], i: usize| bm[i / 8] >> (i % 8) & 1 == 1;
        for ch in self.run_hint..self.chunk_count() {
            if self.chunk_free(ch) == 0 {
                continue;
            }
            let blocks = self.chunk_blocks(ch) as usize;
            let bm = self.bitmap(c, ch)?;
            let mut i = 0;
            while i < blocks && used(bm, i) {
                i = if i % 8 == 0 && bm[i / 8] == 0xff { i + 8 } else { i + 1 };
            }
            if i >= blocks {
                continue;
            }
            let mut j = i;
            while j < blocks && ((j - i) as u64) < max && !used(bm, j) {
                bm[j / 8] |= 1 << (j % 8);
                j += 1;
            }
            self.add_chunk_free(ch, -((j - i) as i64));
            self.run_hint = ch;
            return Ok((ch as u64 * bpc + i as u64, (j - i) as u64));
        }
        Err(Error::NoSpace("the container is full".into()))
    }

    /// The first free run of at least `min_run` blocks (in a chunk with that
    /// much free), taking up to `max` of it.
    fn find_big_run(&mut self, c: &Container, min_run: u64, max: u64) -> Result<Option<(u64, u64)>> {
        let bpc = self.sm.blocks_per_chunk as u64;
        for ch in self.big_hint..self.chunk_count() {
            if (self.chunk_free(ch) as u64) < min_run {
                continue;
            }
            let blocks = self.chunk_blocks(ch) as usize;
            let bm = self.bitmap(c, ch)?;
            let used = |i: usize| bm[i / 8] >> (i % 8) & 1 == 1;
            let mut found = None;
            let mut i = 0;
            while i < blocks {
                if used(i) {
                    i = if i % 8 == 0 && bm[i / 8] == 0xff { i + 8 } else { i + 1 };
                    continue;
                }
                let start = i;
                while i < blocks && ((i - start) as u64) < max && !used(i) {
                    let whole_byte = i % 8 == 0 && i + 8 <= blocks && bm[i / 8] == 0 && ((i + 8 - start) as u64) <= max;
                    i += if whole_byte { 8 } else { 1 };
                }
                if (i - start) as u64 >= min_run {
                    found = Some((start, i - start));
                    break;
                }
            }
            if let Some((start, len)) = found {
                let bm = self.bitmaps.get_mut(&ch).unwrap();
                for j in start..start + len {
                    bm[j / 8] |= 1 << (j % 8);
                }
                self.add_chunk_free(ch, -(len as i64));
                self.big_hint = ch;
                return Ok(Some((ch as u64 * bpc + start as u64, len as u64)));
            }
        }
        Ok(None)
    }

    /// Queues a main-device block for freeing.
    pub fn free(&mut self, paddr: u64) {
        self.free_run(paddr, 1);
    }

    /// Queues `count` main-device blocks from `paddr` for freeing.
    pub fn free_run(&mut self, paddr: u64, count: u64) {
        self.fq[SFQ_MAIN].push(FqEntry { xid: self.xid, paddr, count });
    }

    /// Free main-device blocks, after this transaction's allocations.
    pub fn free_blocks(&self) -> u64 {
        self.sm.dev[SD_MAIN].free_count
    }

    /// Frees queued so far.
    pub fn queued_frees(&self) -> usize {
        self.fq.iter().map(Vec::len).sum()
    }

    /// Where a transaction should stop queueing frees and commit: half of
    /// what fits under the free queues' node limits (and well inside one
    /// checkpoint-map block), leaving room for the frees the commit adds.
    pub fn queued_free_limit(&self) -> usize {
        // A leaf entry: a 16-byte key, an 8-byte count, a 4-byte table slot.
        let per_leaf = (self.bs as usize - 56) / 28;
        let leaves = [SFQ_IP, SFQ_MAIN]
            .iter()
            .map(|&q| self.sm.fq[q].tree_node_limit as usize)
            .filter(|&l| l > 1)
            .fold(61, usize::min)
            - 1;
        leaves * per_leaf / 2
    }

    fn ip_alloc(&mut self) -> Result<u64> {
        let n = self.sm.ip_block_count as usize;
        let i = (0..n)
            .find(|&i| self.ip_bitmap[i / 8] >> (i % 8) & 1 == 0)
            .ok_or_else(|| Error::NoSpace("the space manager's internal pool is full".into()))?;
        self.ip_bitmap[i / 8] |= 1 << (i % 8);
        self.ip_dirty[i / (self.bs as usize * 8)] = true;
        Ok(self.sm.ip_base + i as u64)
    }

    fn ip_free(&mut self, paddr: u64) {
        self.fq[SFQ_IP].push(FqEntry { xid: self.xid, paddr, count: 1 });
    }

    /// Releases blocks freed by earlier transactions: no checkpoint that
    /// could still be mounted refers to them any more.
    pub fn release_queued(&mut self, c: &Container, durable: u64) -> Result<()> {
        // A block freed in transaction x is unused from checkpoint x on, but
        // checkpoint x-1 still uses it: it is free once x is on the disk.
        let ip_old: Vec<FqEntry> = self.fq[SFQ_IP].iter().filter(|e| e.xid <= durable).copied().collect();
        self.fq[SFQ_IP].retain(|e| e.xid > durable);
        for e in ip_old {
            for b in e.paddr..e.paddr + e.count {
                let i = (b - self.sm.ip_base) as usize;
                self.ip_bitmap[i / 8] &= !(1 << (i % 8));
                self.ip_dirty[i / (self.bs as usize * 8)] = true;
            }
        }
        let main_old: Vec<FqEntry> = self.fq[SFQ_MAIN].iter().filter(|e| e.xid <= durable).copied().collect();
        self.fq[SFQ_MAIN].retain(|e| e.xid > durable);
        let bpc = self.sm.blocks_per_chunk as u64;
        for e in main_old {
            for b in e.paddr..e.paddr + e.count {
                let ch = (b / bpc) as usize;
                let i = (b % bpc) as usize;
                let bm = self.bitmap(c, ch)?;
                bm[i / 8] &= !(1 << (i % 8));
                self.add_chunk_free(ch, 1);
            }
        }
        Ok(())
    }

    /// Writes the changed bitmaps, CIBs and internal-pool bitmap (appending
    /// them to `writes`) and returns the new ephemeral objects (the space
    /// manager and the free-queue trees' nodes, which may take new ids from
    /// `next_oid`) and the ids of ephemeral objects no longer used.
    pub fn finish(mut self, writes: &mut Vec<(u64, Vec<u8>)>, next_oid: &mut u64) -> Result<(Vec<EphObject>, Vec<u64>)> {
        let xid = self.xid;
        for ch in std::mem::take(&mut self.dirty_chunks) {
            let new = self.ip_alloc()?;
            writes.push((new, self.bitmaps[&ch].clone()));
            let old = self.chunk_bitmap_addr(ch);
            if old != 0 {
                self.ip_free(old);
            }
            let (cib, e) = self.chunk_pos(ch);
            put_u64(&mut self.cibs[cib].raw, e, xid);
            put_u64(&mut self.cibs[cib].raw, e + 24, new);
            self.cibs[cib].dirty = true;
        }
        for i in 0..self.cibs.len() {
            if !self.cibs[i].dirty {
                continue;
            }
            let new = self.ip_alloc()?;
            let old = self.cibs[i].paddr;
            self.ip_free(old);
            let raw = &mut self.cibs[i].raw;
            put_u64(raw, 8, new); // physical object: its id is its address
            set_xid(raw, xid);
            seal(raw);
            writes.push((new, raw.clone()));
            let off = self.sm.dev[SD_MAIN].addr_offset as usize + i * 8;
            put_u64(&mut self.sm_raw, off, new);
        }
        for j in 0..self.ip_dirty.len() {
            if self.ip_dirty[j] {
                self.rotate_ip_bitmap(writes, j)?;
            }
        }

        // Fixed fields of the space manager.
        put_u64(&mut self.sm_raw, 48 + 24, self.sm.dev[SD_MAIN].free_count);
        for (slot, q) in [SFQ_IP, SFQ_MAIN].into_iter().enumerate() {
            let entries = &self.fq[slot];
            let o = 200 + q * 40;
            put_u64(&mut self.sm_raw, o, entries.iter().map(|e| e.count).sum());
            put_u64(&mut self.sm_raw, o + 16, entries.iter().map(|e| e.xid).min().unwrap_or(0));
        }
        set_xid(&mut self.sm_raw, xid);
        seal(&mut self.sm_raw);
        let mut out = vec![EphObject {
            oid: self.sm.hdr.oid,
            obj_type: self.sm.hdr.obj_type,
            subtype: self.sm.hdr.subtype,
            data: self.sm_raw.clone(),
        }];

        // Node ids for free-queue leaves: the old non-root nodes' first.
        let mut spare = std::mem::take(&mut self.fq_old_nodes);
        spare.reverse();
        for (slot, q) in [SFQ_IP, SFQ_MAIN].into_iter().enumerate() {
            let Some((root_type, info)) = self.fq_roots[slot] else { continue };
            let mut entries = self.fq[slot].clone();
            entries.sort();
            let kv: Vec<Entry> = entries
                .iter()
                .map(|e| {
                    let mut k = e.xid.to_le_bytes().to_vec();
                    k.extend(e.paddr.to_le_bytes());
                    // A ghost (no value) means a count of one.
                    (k, (e.count != 1).then(|| e.count.to_le_bytes().to_vec()))
                })
                .collect();
            let tree_oid = self.sm.fq[q].tree_oid;
            let node_type = (root_type & !crate::obj::OBJECT_TYPE_MASK) | crate::obj::OBJECT_TYPE_BTREE_NODE;
            let spec = |oid: u64, obj_type: u32, level: u16, root: bool, info: BTreeInfo, entries: &[Entry]| -> Result<Vec<u8>> {
                build(
                    &NodeBuild {
                        oid,
                        xid,
                        obj_type,
                        subtype: crate::obj::OBJECT_TYPE_SPACEMAN_FREE_QUEUE,
                        level,
                        root,
                        fixed: Some((16, 8)),
                        info,
                        entries,
                    },
                    self.bs as usize,
                )
            };
            let fits = |root: bool, level: u16, entries: &[Entry]| {
                crate::btree_write::fits(
                    &NodeBuild {
                        oid: 0,
                        xid,
                        obj_type: root_type,
                        subtype: 0,
                        level,
                        root,
                        fixed: Some((16, 8)),
                        info,
                        entries,
                    },
                    self.bs as usize,
                )
            };
            if fits(true, 0, &kv) {
                let info = BTreeInfo { key_count: kv.len() as u64, node_count: 1, ..info };
                out.push(EphObject { oid: tree_oid, obj_type: root_type, subtype: crate::obj::OBJECT_TYPE_SPACEMAN_FREE_QUEUE, data: spec(tree_oid, root_type, 0, true, info, &kv)? });
                continue;
            }
            // Too many for one node: full leaves under one index root.
            let mut leaves: Vec<&[Entry]> = Vec::new();
            let mut rest = &kv[..];
            while !rest.is_empty() {
                let mut n = rest.len();
                while n > 1 && !fits(false, 0, &rest[..n]) {
                    n = (n * 9 / 10).max(1);
                }
                while n < rest.len() && fits(false, 0, &rest[..n + 1]) {
                    n += 1;
                }
                leaves.push(&rest[..n]);
                rest = &rest[n..];
            }
            let node_limit = self.sm.fq[q].tree_node_limit as usize;
            if node_limit != 0 && leaves.len() + 1 > node_limit {
                return Err(unsupported(format!("a free queue of {} nodes (the limit is {node_limit})", leaves.len() + 1)));
            }
            let info = BTreeInfo { key_count: kv.len() as u64, node_count: leaves.len() as u64 + 1, ..info };
            let mut index = Vec::new();
            for leaf in &leaves {
                let oid = match spare.pop() {
                    Some(o) => o,
                    None => {
                        let o = *next_oid;
                        *next_oid += 1;
                        o
                    }
                };
                out.push(EphObject { oid, obj_type: node_type, subtype: crate::obj::OBJECT_TYPE_SPACEMAN_FREE_QUEUE, data: spec(oid, node_type, 0, false, info, leaf)? });
                index.push((leaf[0].0.clone(), Some(oid.to_le_bytes().to_vec())));
            }
            if !fits(true, 1, &index) {
                return Err(unsupported("free queues longer than two B-tree levels"));
            }
            out.push(EphObject { oid: tree_oid, obj_type: root_type, subtype: crate::obj::OBJECT_TYPE_SPACEMAN_FREE_QUEUE, data: spec(tree_oid, root_type, 1, true, info, &index)? });
        }
        // Old nodes not reused leave the checkpoint.
        Ok((out, spare))
    }

    /// Writes the internal-pool bitmap to the next free block of its ring and
    /// returns the previous one to the end of the ring's free list.
    fn rotate_ip_bitmap(&mut self, writes: &mut Vec<(u64, Vec<u8>)>, j: usize) -> Result<()> {
        let sm = &self.sm;
        let (head_off, tail_off) = (320usize, 322usize);
        let next_off = sm.ip_bm_free_next_offset as usize;
        let index_off = sm.ip_bitmap_offset as usize + j * 2;
        let get16 = |raw: &[u8], off: usize| u16::from_le_bytes([raw[off], raw[off + 1]]);
        let set16 = |raw: &mut [u8], off: usize, v: u16| raw[off..off + 2].copy_from_slice(&v.to_le_bytes());
        let old = get16(&self.sm_raw, index_off);
        let new = get16(&self.sm_raw, head_off);
        if new == NO_INDEX {
            return Err(unsupported("the internal-pool bitmap ring has no free block"));
        }
        let after = get16(&self.sm_raw, next_off + new as usize * 2);
        set16(&mut self.sm_raw, head_off, after);
        set16(&mut self.sm_raw, next_off + new as usize * 2, NO_INDEX);
        // Append the old block to the free list's tail (or start the list if
        // taking `new` emptied it).
        set16(&mut self.sm_raw, next_off + old as usize * 2, NO_INDEX);
        if after == NO_INDEX {
            set16(&mut self.sm_raw, head_off, old);
        } else {
            let tail = get16(&self.sm_raw, tail_off);
            set16(&mut self.sm_raw, next_off + tail as usize * 2, old);
        }
        set16(&mut self.sm_raw, tail_off, old);

        set16(&mut self.sm_raw, index_off, new);
        put_u64(&mut self.sm_raw, sm.ip_bm_xid_offset as usize + j * 8, self.xid);
        let bs = self.bs as usize;
        writes.push((sm.ip_bm_base + new as u64, self.ip_bitmap[j * bs..(j + 1) * bs].to_vec()));
        Ok(())
    }
}

pub(crate) struct NodeReader<'a>(pub &'a Container);

impl NodeSource for NodeReader<'_> {
    fn read_node(&self, paddr: u64) -> Result<std::sync::Arc<crate::btree::Node>> {
        self.0.node(paddr)
    }
}
