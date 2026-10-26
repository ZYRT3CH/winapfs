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
use crate::btree_write::{build, NodeBuild};
use crate::bytes::{u32_at, u64_at};
use crate::checksum;
use crate::error::{corrupt, unsupported, Error, Result};
use crate::nx::Container;
use crate::spaceman::{Spaceman, SD_MAIN, SD_TIER2, SFQ_IP, SFQ_MAIN};
use crate::write::{put_u32, put_u64, seal, set_xid};

const NO_INDEX: u16 = 0xffff;

/// A free-queue entry: `count` blocks from `paddr`, freed in transaction `xid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FqEntry {
    pub xid: u64,
    pub paddr: u64,
    pub count: u64,
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
    ip_dirty: bool,
    fq: [Vec<FqEntry>; 2],
    fq_roots: [Option<(u32, BTreeInfo)>; 2],
    /// Chunk where the last run was found; earlier ones were full.
    run_hint: usize,
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
        if sm.ip_bm_size_in_blocks != 1 {
            return Err(unsupported("internal-pool bitmaps larger than one block"));
        }
        let mut cibs = Vec::new();
        for paddr in sm.dev_addrs(SD_MAIN)? {
            let raw = c.raw_block(paddr)?;
            if !checksum::verify(&raw) {
                return Err(corrupt(format!("checksum mismatch in chunk-info block {paddr:#x}")));
            }
            cibs.push(Cib { paddr, raw, dirty: false });
        }
        let ip_index = sm.ip_bitmap_indices()?[0];
        let ip_bitmap = c.raw_block(sm.ip_bm_base + ip_index as u64)?;

        let mut fq: [Vec<FqEntry>; 2] = [Vec::new(), Vec::new()];
        let mut fq_roots = [None, None];
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
            let resolve = |oid: u64| c.ephemeral_paddr(oid).ok_or_else(|| corrupt(format!("free queue node {oid:#x} missing")));
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
            ip_dirty: false,
            fq,
            fq_roots,
            run_hint: 0,
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
            let bm = if addr == 0 { vec![0u8; self.bs as usize] } else { c.raw_block(addr)? };
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
        Err(Error::Unsupported("the container is full".into()))
    }

    /// Allocates up to `max` contiguous main-device blocks: the first free
    /// run, cut at `max` or at the end of its chunk. Returns (first block,
    /// count).
    pub fn alloc_run(&mut self, c: &Container, max: u64) -> Result<(u64, u64)> {
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
        Err(Error::Unsupported("the container is full".into()))
    }

    /// Queues a main-device block for freeing.
    pub fn free(&mut self, paddr: u64) {
        self.free_run(paddr, 1);
    }

    /// Queues `count` main-device blocks from `paddr` for freeing.
    pub fn free_run(&mut self, paddr: u64, count: u64) {
        self.fq[SFQ_MAIN].push(FqEntry { xid: self.xid, paddr, count });
    }

    fn ip_alloc(&mut self) -> Result<u64> {
        let n = self.sm.ip_block_count as usize;
        let i = (0..n)
            .find(|&i| self.ip_bitmap[i / 8] >> (i % 8) & 1 == 0)
            .ok_or_else(|| Error::Unsupported("the space manager's internal pool is full".into()))?;
        self.ip_bitmap[i / 8] |= 1 << (i % 8);
        self.ip_dirty = true;
        Ok(self.sm.ip_base + i as u64)
    }

    fn ip_free(&mut self, paddr: u64) {
        self.fq[SFQ_IP].push(FqEntry { xid: self.xid, paddr, count: 1 });
    }

    /// Releases blocks freed by earlier transactions: no checkpoint that
    /// could still be mounted refers to them any more.
    pub fn release_queued(&mut self, c: &Container) -> Result<()> {
        let xid = self.xid;
        let ip_old: Vec<FqEntry> = self.fq[SFQ_IP].iter().filter(|e| e.xid < xid).copied().collect();
        self.fq[SFQ_IP].retain(|e| e.xid >= xid);
        for e in ip_old {
            for b in e.paddr..e.paddr + e.count {
                let i = (b - self.sm.ip_base) as usize;
                self.ip_bitmap[i / 8] &= !(1 << (i % 8));
                self.ip_dirty = true;
            }
        }
        let main_old: Vec<FqEntry> = self.fq[SFQ_MAIN].iter().filter(|e| e.xid < xid).copied().collect();
        self.fq[SFQ_MAIN].retain(|e| e.xid >= xid);
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
    /// them to `writes`) and returns the new ephemeral objects: the space
    /// manager and the free-queue trees, as (oid, bytes).
    pub fn finish(mut self, writes: &mut Vec<(u64, Vec<u8>)>) -> Result<Vec<(u64, Vec<u8>)>> {
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
        if self.ip_dirty {
            self.rotate_ip_bitmap(writes)?;
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
        let mut out = vec![(self.sm.hdr.oid, self.sm_raw.clone())];

        for (slot, q) in [SFQ_IP, SFQ_MAIN].into_iter().enumerate() {
            let Some((obj_type, info)) = self.fq_roots[slot] else { continue };
            let mut entries = self.fq[slot].clone();
            entries.sort();
            let kv: Vec<_> = entries
                .iter()
                .map(|e| {
                    let mut k = e.xid.to_le_bytes().to_vec();
                    k.extend(e.paddr.to_le_bytes());
                    // A ghost (no value) means a count of one.
                    (k, (e.count != 1).then(|| e.count.to_le_bytes().to_vec()))
                })
                .collect();
            let info = BTreeInfo { key_count: kv.len() as u64, node_count: 1, ..info };
            let nb = NodeBuild {
                oid: self.sm.fq[q].tree_oid,
                xid,
                obj_type,
                subtype: crate::obj::OBJECT_TYPE_SPACEMAN_FREE_QUEUE,
                level: 0,
                root: true,
                fixed: Some((16, 8)),
                info,
                entries: &kv,
            };
            if !crate::btree_write::fits(&nb, self.bs as usize) {
                return Err(unsupported("free queues longer than one B-tree node"));
            }
            out.push((self.sm.fq[q].tree_oid, build(&nb, self.bs as usize)?));
        }
        Ok(out)
    }

    /// Writes the internal-pool bitmap to the next free block of its ring and
    /// returns the previous one to the end of the ring's free list.
    fn rotate_ip_bitmap(&mut self, writes: &mut Vec<(u64, Vec<u8>)>) -> Result<()> {
        let sm = &self.sm;
        let (head_off, tail_off) = (320usize, 322usize);
        let next_off = sm.ip_bm_free_next_offset as usize;
        let get16 = |raw: &[u8], off: usize| u16::from_le_bytes([raw[off], raw[off + 1]]);
        let set16 = |raw: &mut [u8], off: usize, v: u16| raw[off..off + 2].copy_from_slice(&v.to_le_bytes());
        let old = get16(&self.sm_raw, sm.ip_bitmap_offset as usize);
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

        set16(&mut self.sm_raw, sm.ip_bitmap_offset as usize, new);
        put_u64(&mut self.sm_raw, sm.ip_bm_xid_offset as usize, self.xid);
        writes.push((sm.ip_bm_base + new as u64, self.ip_bitmap.clone()));
        Ok(())
    }
}

pub(crate) struct NodeReader<'a>(pub &'a Container);

impl NodeSource for NodeReader<'_> {
    fn read_node(&self, paddr: u64) -> Result<std::sync::Arc<crate::btree::Node>> {
        self.0.node(paddr)
    }
}
