//! The space manager (`spaceman_phys_t`): which blocks are free, organised as
//! chunks of bitmap blocks, plus free queues of blocks released by recent
//! transactions. Layout from the Apple File System Reference; the reference
//! documents the fields but not their use, so the comments here record what
//! was observed on volumes written by Apple's and other implementations.

use crate::bytes::{slice_at, u16_at, u32_at, u64_at};
use crate::error::{corrupt, Result};
use crate::obj::ObjHeader;

pub const SD_MAIN: usize = 0;
pub const SD_TIER2: usize = 1;
pub const SFQ_IP: usize = 0;
pub const SFQ_MAIN: usize = 1;
pub const SFQ_TIER2: usize = 2;

/// `chunk_info_t::ci_free_count` and `ci_block_count` share a word with reserved bits.
pub const CI_COUNT_MASK: u32 = 0x000f_ffff;

#[derive(Debug, Clone, Copy, Default)]
pub struct SpacemanDevice {
    pub block_count: u64,
    pub chunk_count: u64,
    pub cib_count: u32,
    pub cab_count: u32,
    pub free_count: u64,
    /// Byte offset, within the space manager object, of this device's array
    /// of chunk-info-block (or, with `cab_count > 0`, cib-address-block) addresses.
    pub addr_offset: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FreeQueue {
    pub count: u64,
    pub tree_oid: u64,
    pub oldest_xid: u64,
    pub tree_node_limit: u16,
}

#[derive(Debug, Clone)]
pub struct Spaceman {
    pub hdr: ObjHeader,
    pub block_size: u32,
    pub blocks_per_chunk: u32,
    pub chunks_per_cib: u32,
    pub cibs_per_cab: u32,
    pub dev: [SpacemanDevice; 2],
    pub flags: u32,
    pub ip_bm_tx_multiplier: u32,
    pub ip_block_count: u64,
    pub ip_bm_size_in_blocks: u32,
    pub ip_bm_block_count: u32,
    pub ip_bm_base: u64,
    pub ip_base: u64,
    pub fs_reserve_block_count: u64,
    pub fs_reserve_alloc_count: u64,
    pub fq: [FreeQueue; 3],
    pub ip_bm_free_head: u16,
    pub ip_bm_free_tail: u16,
    pub ip_bm_xid_offset: u32,
    pub ip_bitmap_offset: u32,
    pub ip_bm_free_next_offset: u32,
    pub version: u32,
    pub struct_size: u32,
    /// The whole object, which may span several blocks.
    pub raw: Vec<u8>,
}

/// One `chunk_info_t`: a run of `block_count` blocks starting at `addr`,
/// tracked by the bitmap block at `bitmap_addr` (0 = all free).
#[derive(Debug, Clone, Copy)]
pub struct ChunkInfo {
    pub xid: u64,
    pub addr: u64,
    pub block_count: u32,
    pub free_count: u32,
    pub bitmap_addr: u64,
}

impl Spaceman {
    pub fn parse(b: &[u8]) -> Result<Self> {
        let dev = |i: usize| -> Result<SpacemanDevice> {
            let o = 48 + i * 48;
            Ok(SpacemanDevice {
                block_count: u64_at(b, o)?,
                chunk_count: u64_at(b, o + 8)?,
                cib_count: u32_at(b, o + 16)?,
                cab_count: u32_at(b, o + 20)?,
                free_count: u64_at(b, o + 24)?,
                addr_offset: u32_at(b, o + 32)?,
            })
        };
        let fq = |i: usize| -> Result<FreeQueue> {
            let o = 200 + i * 40;
            Ok(FreeQueue {
                count: u64_at(b, o)?,
                tree_oid: u64_at(b, o + 8)?,
                oldest_xid: u64_at(b, o + 16)?,
                tree_node_limit: u16_at(b, o + 24)?,
            })
        };
        Ok(Self {
            hdr: ObjHeader::parse(b)?,
            block_size: u32_at(b, 32)?,
            blocks_per_chunk: u32_at(b, 36)?,
            chunks_per_cib: u32_at(b, 40)?,
            cibs_per_cab: u32_at(b, 44)?,
            dev: [dev(0)?, dev(1)?],
            flags: u32_at(b, 144)?,
            ip_bm_tx_multiplier: u32_at(b, 148)?,
            ip_block_count: u64_at(b, 152)?,
            ip_bm_size_in_blocks: u32_at(b, 160)?,
            ip_bm_block_count: u32_at(b, 164)?,
            ip_bm_base: u64_at(b, 168)?,
            ip_base: u64_at(b, 176)?,
            fs_reserve_block_count: u64_at(b, 184)?,
            fs_reserve_alloc_count: u64_at(b, 192)?,
            fq: [fq(0)?, fq(1)?, fq(2)?],
            ip_bm_free_head: u16_at(b, 320)?,
            ip_bm_free_tail: u16_at(b, 322)?,
            ip_bm_xid_offset: u32_at(b, 324)?,
            ip_bitmap_offset: u32_at(b, 328)?,
            ip_bm_free_next_offset: u32_at(b, 332)?,
            version: u32_at(b, 336)?,
            struct_size: u32_at(b, 340)?,
            raw: b.to_vec(),
        })
    }

    /// Addresses of device `d`'s chunk-info blocks, or of its cib-address
    /// blocks when the device is large enough to need them.
    pub fn dev_addrs(&self, d: usize) -> Result<Vec<u64>> {
        let dv = &self.dev[d];
        let n = if dv.cab_count > 0 { dv.cab_count } else { dv.cib_count } as usize;
        (0..n).map(|i| u64_at(&self.raw, dv.addr_offset as usize + i * 8)).collect()
    }

    /// Per internal-pool bitmap slot: the transaction that last wrote it.
    pub fn ip_bm_xids(&self) -> Result<Vec<u64>> {
        (0..self.ip_bm_size_in_blocks as usize).map(|i| u64_at(&self.raw, self.ip_bm_xid_offset as usize + i * 8)).collect()
    }

    /// Per internal-pool bitmap slot: which block of the bitmap ring holds it.
    pub fn ip_bitmap_indices(&self) -> Result<Vec<u16>> {
        (0..self.ip_bm_size_in_blocks as usize).map(|i| u16_at(&self.raw, self.ip_bitmap_offset as usize + i * 2)).collect()
    }

    /// Free list over the internal-pool bitmap ring: next index per block.
    pub fn ip_bm_free_next(&self) -> Result<Vec<u16>> {
        (0..self.ip_bm_block_count as usize)
            .map(|i| u16_at(&self.raw, self.ip_bm_free_next_offset as usize + i * 2))
            .collect()
    }
}

/// Parses a chunk-info block (`chunk_info_block_t`).
pub fn chunk_infos(b: &[u8]) -> Result<(u32, Vec<ChunkInfo>)> {
    let index = u32_at(b, 32)?;
    let count = u32_at(b, 36)? as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let e = slice_at(b, 40 + i * 32, 32)?;
        out.push(ChunkInfo {
            xid: u64_at(e, 0)?,
            addr: u64_at(e, 8)?,
            block_count: u32_at(e, 16)? & CI_COUNT_MASK,
            free_count: u32_at(e, 20)? & CI_COUNT_MASK,
            bitmap_addr: u64_at(e, 24)?,
        });
    }
    if out.len() != count {
        return Err(corrupt("short chunk-info block"));
    }
    Ok((index, out))
}

/// Parses a cib-address block (`cib_addr_block_t`).
pub fn cib_addrs(b: &[u8]) -> Result<(u32, Vec<u64>)> {
    let index = u32_at(b, 32)?;
    let count = u32_at(b, 36)? as usize;
    Ok((index, (0..count).map(|i| u64_at(b, 40 + i * 8)).collect::<Result<_>>()?))
}
