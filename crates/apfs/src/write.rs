//! Writing to APFS containers. EXPERIMENTAL, and limited to disk image files
//! (see [`ImageFile`]): every step is checked against independent checkers on
//! throwaway images before it may ever touch a real disk.
//!
//! APFS never overwrites live metadata: a transaction writes new copies of
//! what it changes, then a new checkpoint (checkpoint-map blocks plus a new
//! container superblock) in the checkpoint rings. Until that superblock is
//! written the previous checkpoint stays the newest valid one, so a crash
//! mid-transaction loses the transaction, not the container.

use std::path::Path;

use crate::bytes::u32_at;
use crate::checksum;
use crate::dev::{FileDevice, ImageFile, WriteDevice};
use crate::error::{corrupt, unsupported, Result};
use crate::gpt;
use crate::nx::{CheckpointMapping, Container};
use crate::obj::{OBJ_PHYSICAL, OBJECT_TYPE_CHECKPOINT_MAP};

const CHECKPOINT_MAP_LAST: u32 = 0x1;
/// `NX_RESERVED_2`: "remove it when modifying the container".
const NX_RESERVED_2: u64 = 0x2;

/// Sets an object's transaction id.
pub(crate) fn set_xid(obj: &mut [u8], xid: u64) {
    obj[16..24].copy_from_slice(&xid.to_le_bytes());
}

/// Stores a fresh checksum in an object's header.
pub(crate) fn seal(obj: &mut [u8]) {
    let c = checksum::compute(obj);
    obj[0..8].copy_from_slice(&c.to_le_bytes());
}

pub(crate) fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// An APFS container in an image file, open for writing.
pub struct Writer {
    c: Container,
    dev: ImageFile,
    path: std::path::PathBuf,
    base: u64,
}

/// Byte offset of the APFS container in an image: its GPT partition, or 0.
fn container_offset(path: &Path) -> Result<u64> {
    let dev = FileDevice::open(path)?;
    Ok(match gpt::read_partitions(&dev)? {
        Some(parts) => parts.iter().find(|p| p.is_apfs()).ok_or_else(|| corrupt("no APFS partition in the image"))?.offset,
        None => 0,
    })
}

impl Writer {
    pub fn open(path: &Path) -> Result<Self> {
        let base = container_offset(path)?;
        let dev = ImageFile::open_rw(path)?.with_base(base)?;
        let c = Container::open(Box::new(FileDevice::open(path)?.with_base(base)?))?;
        Ok(Self { c, dev, path: path.to_path_buf(), base })
    }

    /// The committed state, as a reader sees it.
    pub fn container(&self) -> &Container {
        &self.c
    }

    /// Re-reads the committed state after a commit.
    fn reload(&mut self) -> Result<()> {
        self.c = Container::open(Box::new(FileDevice::open(&self.path)?.with_base(self.base)?))?;
        Ok(())
    }

    /// Writes a new checkpoint that records the current state unchanged.
    /// Returns its transaction id.
    pub fn commit_empty(&mut self) -> Result<u64> {
        let sb = &self.c.sb;
        if sb.xp_desc_blocks & 0x8000_0000 != 0 || sb.xp_data_blocks & 0x8000_0000 != 0 {
            return Err(unsupported("non-contiguous checkpoint areas"));
        }
        let bs = self.c.block_size() as u64;
        let xid = sb.next_xid;
        let (desc_base, desc_n) = (sb.xp_desc_base, sb.xp_desc_blocks as u64);
        let (data_base, data_n) = (sb.xp_data_base, sb.xp_data_blocks as u64);

        // Ephemeral objects go into the data ring right after the current
        // checkpoint's, so the current checkpoint stays intact.
        let data_start = sb.xp_data_next as u64 % data_n;
        let mut data_pos = data_start;
        let mut writes: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut mappings: Vec<CheckpointMapping> = Vec::new();
        for m in &self.c.checkpoint_map {
            let mut obj = self.c.ephemeral(m.oid)?;
            set_xid(&mut obj, xid);
            seal(&mut obj);
            obj.resize((obj.len() as u64).div_ceil(bs) as usize * bs as usize, 0);
            let first = data_base + data_pos % data_n;
            for block in obj.chunks(bs as usize) {
                writes.push((data_base + data_pos % data_n, block.to_vec()));
                data_pos += 1;
            }
            mappings.push(CheckpointMapping { paddr: first, ..*m });
        }
        let data_len = data_pos - data_start;
        if data_len + sb.xp_data_len as u64 > data_n {
            return Err(unsupported("checkpoint data area too small for another checkpoint"));
        }

        // One checkpoint-map block, then the superblock, in the descriptor ring.
        let entries_per_block = (bs as usize - 40) / 40;
        if mappings.len() > entries_per_block {
            return Err(unsupported("checkpoints that need several checkpoint-map blocks"));
        }
        if 2 + sb.xp_desc_len as u64 > desc_n {
            return Err(unsupported("checkpoint descriptor area too small for another checkpoint"));
        }
        let cpm_pos = sb.xp_desc_next as u64 % desc_n;
        let sb_pos = (cpm_pos + 1) % desc_n;
        let cpm_paddr = desc_base + cpm_pos;
        let mut cpm = vec![0u8; bs as usize];
        put_u64(&mut cpm, 8, cpm_paddr); // a physical object's id is its address
        put_u64(&mut cpm, 16, xid);
        put_u32(&mut cpm, 24, OBJ_PHYSICAL | OBJECT_TYPE_CHECKPOINT_MAP);
        put_u32(&mut cpm, 32, CHECKPOINT_MAP_LAST);
        put_u32(&mut cpm, 36, mappings.len() as u32);
        for (i, m) in mappings.iter().enumerate() {
            let e = 40 + i * 40;
            put_u32(&mut cpm, e, m.obj_type);
            put_u32(&mut cpm, e + 4, m.subtype);
            put_u32(&mut cpm, e + 8, m.size);
            put_u64(&mut cpm, e + 16, m.fs_oid);
            put_u64(&mut cpm, e + 24, m.oid);
            put_u64(&mut cpm, e + 32, m.paddr);
        }
        seal(&mut cpm);

        let mut nsb = sb.raw.clone();
        set_xid(&mut nsb, xid);
        put_u64(&mut nsb, 96, xid + 1); // nx_next_xid
        put_u32(&mut nsb, 128, ((sb_pos + 1) % desc_n) as u32); // nx_xp_desc_next
        put_u32(&mut nsb, 132, (data_pos % data_n) as u32); // nx_xp_data_next
        put_u32(&mut nsb, 136, cpm_pos as u32); // nx_xp_desc_index
        put_u32(&mut nsb, 140, 2); // nx_xp_desc_len
        put_u32(&mut nsb, 144, data_start as u32); // nx_xp_data_index
        put_u32(&mut nsb, 148, data_len as u32); // nx_xp_data_len
        put_u64(&mut nsb, 1264, sb.flags & !NX_RESERVED_2);
        seal(&mut nsb);
        debug_assert_eq!(u32_at(&nsb, 32).ok(), Some(crate::nx::NX_MAGIC));

        // Ordering is what makes this crash-safe: the new superblock is
        // written only after everything it points to is durable.
        for (paddr, block) in &writes {
            self.dev.write_at(paddr * bs, block)?;
        }
        self.dev.write_at(cpm_paddr * bs, &cpm)?;
        self.dev.flush()?;
        self.dev.write_at((desc_base + sb_pos) * bs, &nsb)?;
        self.dev.flush()?;
        self.dev.write_at(0, &nsb)?;
        self.dev.flush()?;

        self.reload()?;
        if self.c.xid() != xid {
            return Err(corrupt(format!("after committing xid {xid} the newest checkpoint is {}", self.c.xid())));
        }
        Ok(xid)
    }
}
