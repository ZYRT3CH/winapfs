//! Writing to APFS containers. EXPERIMENTAL, and limited to disk image files
//! (see [`ImageFile`]): every step is checked against independent checkers on
//! throwaway images before it may ever touch a real disk.
//!
//! APFS never overwrites live metadata: a transaction writes new copies of
//! what it changes, then a new checkpoint (checkpoint-map blocks plus a new
//! container superblock) in the checkpoint rings. Until that superblock is
//! written the previous checkpoint stays the newest valid one, so a crash
//! mid-transaction loses the transaction, not the container.

mod space;

use std::collections::HashMap;
use std::path::Path;

use crate::btree::BTreeInfo;
use crate::btree_write::{build, fits, NodeBuild};
use crate::bytes::u64_at;
use crate::checksum;
use crate::dev::{FileDevice, ImageFile, WriteDevice};
use crate::error::{corrupt, unsupported, Result};
use crate::gpt;
use crate::nx::{CheckpointMapping, Container};
use crate::obj::{OBJ_PHYSICAL, OBJECT_TYPE_CHECKPOINT_MAP};
use space::Space;

const CHECKPOINT_MAP_LAST: u32 = 0x1;
/// `NX_RESERVED_2`: "remove it when modifying the container".
const NX_RESERVED_2: u64 = 0x2;
/// Shown in a volume's `apfs_modified_by` history.
const MODIFIED_BY: &str = concat!("winapfs ", env!("CARGO_PKG_VERSION"));

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

fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// An APFS container in an image file, open for writing.
pub struct Writer {
    c: Container,
    dev: ImageFile,
    path: std::path::PathBuf,
    base: u64,
}

/// One transaction: block allocations, new object copies and superblock
/// changes, all applied together by [`Writer::commit`].
pub struct Txn {
    pub xid: u64,
    space: Space,
    writes: Vec<(u64, Vec<u8>)>,
    nx_omap_oid: Option<u64>,
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

    fn reload(&mut self) -> Result<()> {
        self.c = Container::open(Box::new(FileDevice::open(&self.path)?.with_base(self.base)?))?;
        Ok(())
    }

    /// Starts a transaction. Blocks that earlier transactions freed become
    /// reusable now.
    pub fn begin(&self) -> Result<Txn> {
        let xid = self.c.sb.next_xid;
        let mut space = Space::load(&self.c, xid)?;
        space.release_queued(&self.c)?;
        Ok(Txn { xid, space, writes: Vec::new(), nx_omap_oid: None })
    }

    /// Writes a new checkpoint that records the current state unchanged.
    pub fn commit_empty(&mut self) -> Result<u64> {
        let t = self.begin()?;
        self.commit(t)
    }

    /// Renames volume `index` (a small change that still exercises the
    /// object map, the space manager and the free queues).
    pub fn rename_volume(&mut self, index: usize, name: &str) -> Result<u64> {
        if name.len() >= 256 || name.contains('\0') {
            return Err(unsupported("volume names of 256 bytes or more"));
        }
        let mut t = self.begin()?;
        let oid = *self.c.sb.fs_oids.get(index).ok_or_else(|| corrupt(format!("no volume {index}")))?;
        let old = self.c.omap_lookup(oid)?;
        let mut apsb = self.c.raw_block(old)?;
        if !checksum::verify(&apsb) {
            return Err(corrupt("volume superblock checksum"));
        }
        apsb[704..960].fill(0);
        apsb[704..704 + name.len()].copy_from_slice(name.as_bytes());
        stamp_modified_by(&mut apsb, t.xid);
        set_xid(&mut apsb, t.xid);
        seal(&mut apsb);
        let new = t.space.alloc(&self.c)?;
        t.writes.push((new, apsb));
        t.space.free(old);
        let omap = self.omap_set(&mut t, self.c.sb.omap_oid, oid, new)?;
        t.nx_omap_oid = Some(omap);
        self.commit(t)
    }

    /// Points `oid` at `paddr` (as of this transaction) in the object map at
    /// `omap_paddr`, dropping its older versions; returns the object map's new
    /// address. Copy-on-write: the tree node and the object map itself move.
    fn omap_set(&self, t: &mut Txn, omap_paddr: u64, oid: u64, paddr: u64) -> Result<u64> {
        let mut phys = self.c.raw_block(omap_paddr)?;
        if !checksum::verify(&phys) {
            return Err(corrupt("object map checksum"));
        }
        let tree_paddr = u64_at(&phys, 48)?;
        let root = self.c.node(tree_paddr)?;
        if !root.is_leaf() {
            return Err(unsupported("object maps deeper than one node"));
        }
        let info = root.info()?;
        let mut entries = root.all_entries(&info)?;
        entries.retain(|(k, _)| u64_at(k, 0).ok() != Some(oid));
        let mut key = oid.to_le_bytes().to_vec();
        key.extend(t.xid.to_le_bytes());
        let mut val = 0u32.to_le_bytes().to_vec(); // flags
        val.extend((self.c.block_size()).to_le_bytes()); // size
        val.extend(paddr.to_le_bytes());
        let at = entries.partition_point(|(k, _)| k[..] < key[..]);
        entries.insert(at, (key, Some(val)));

        let new_root = t.space.alloc(&self.c)?;
        let info = BTreeInfo { key_count: entries.len() as u64, node_count: 1, ..info };
        let nb = NodeBuild {
            oid: new_root,
            xid: t.xid,
            obj_type: root.hdr.obj_type,
            subtype: root.hdr.subtype,
            level: 0,
            root: true,
            fixed: Some((16, 16)),
            info,
            entries: &entries,
        };
        if !fits(&nb, self.c.block_size() as usize) {
            return Err(unsupported("object maps that outgrow one node"));
        }
        t.writes.push((new_root, build(&nb, self.c.block_size() as usize)?));
        t.space.free(tree_paddr);

        let new_phys = t.space.alloc(&self.c)?;
        put_u64(&mut phys, 8, new_phys);
        set_xid(&mut phys, t.xid);
        put_u64(&mut phys, 48, new_root);
        seal(&mut phys);
        t.writes.push((new_phys, phys));
        t.space.free(omap_paddr);
        Ok(new_phys)
    }

    /// Commits a transaction: writes its blocks, then a new checkpoint.
    /// Returns the transaction id.
    pub fn commit(&mut self, t: Txn) -> Result<u64> {
        let Txn { xid, space, mut writes, nx_omap_oid } = t;
        let ephemeral: HashMap<u64, Vec<u8>> = space.finish(&mut writes)?.into_iter().collect();

        let sb = &self.c.sb;
        if sb.xp_desc_blocks & 0x8000_0000 != 0 || sb.xp_data_blocks & 0x8000_0000 != 0 {
            return Err(unsupported("non-contiguous checkpoint areas"));
        }
        let bs = self.c.block_size() as u64;
        let (desc_base, desc_n) = (sb.xp_desc_base, sb.xp_desc_blocks as u64);
        let (data_base, data_n) = (sb.xp_data_base, sb.xp_data_blocks as u64);

        // Ephemeral objects go into the data ring right after the current
        // checkpoint's, so the current checkpoint stays intact.
        let data_start = sb.xp_data_next as u64 % data_n;
        let mut data_pos = data_start;
        let mut eph_writes: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut mappings: Vec<CheckpointMapping> = Vec::new();
        for m in &self.c.checkpoint_map {
            let mut obj = match ephemeral.get(&m.oid) {
                Some(updated) => updated.clone(),
                None => {
                    let mut o = self.c.ephemeral(m.oid)?;
                    set_xid(&mut o, xid);
                    seal(&mut o);
                    o
                }
            };
            obj.resize((obj.len() as u64).div_ceil(bs) as usize * bs as usize, 0);
            let first = data_base + data_pos % data_n;
            for block in obj.chunks(bs as usize) {
                eph_writes.push((data_base + data_pos % data_n, block.to_vec()));
                data_pos += 1;
            }
            mappings.push(CheckpointMapping { paddr: first, ..*m });
        }
        let data_len = data_pos - data_start;
        if data_len + sb.xp_data_len as u64 > data_n {
            return Err(unsupported("checkpoint data area too small for another checkpoint"));
        }

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
        if let Some(omap) = nx_omap_oid {
            put_u64(&mut nsb, 160, omap);
        }
        put_u64(&mut nsb, 1264, sb.flags & !NX_RESERVED_2);
        seal(&mut nsb);

        // Ordering is what makes this crash-safe: the new superblock is
        // written only after everything it points to is durable.
        for (paddr, block) in writes.iter().chain(&eph_writes) {
            if *paddr == 0 || *paddr >= sb.block_count {
                return Err(corrupt(format!("refusing to write outside the container: block {paddr:#x}")));
            }
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

/// Records this program in a volume superblock's modification history
/// (`apfs_modified_by`, newest first) and its last-modified time.
fn stamp_modified_by(apsb: &mut [u8], xid: u64) {
    const HISTORY: usize = 320;
    const ENTRY: usize = 48;
    let now = now_ns();
    let mut entry = [0u8; ENTRY];
    entry[..MODIFIED_BY.len()].copy_from_slice(MODIFIED_BY.as_bytes());
    entry[32..40].copy_from_slice(&now.to_le_bytes());
    entry[40..48].copy_from_slice(&xid.to_le_bytes());
    // Same program as the newest entry: update it; otherwise push a new one.
    if apsb[HISTORY..HISTORY + 32] != entry[..32] {
        apsb.copy_within(HISTORY..HISTORY + 7 * ENTRY, HISTORY + ENTRY);
    }
    apsb[HISTORY..HISTORY + ENTRY].copy_from_slice(&entry);
    put_u64(apsb, 256, now); // apfs_last_mod_time
}
