//! The container (`nx_superblock_t`), its checkpoints and its volume list.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::btree::{Node, NodeSource};
use crate::bytes::{cstr, slice_at, u16_at, u32_at, u64_at, uuid_at};
use crate::checksum;
use crate::dev::BlockDevice;
use crate::error::{corrupt, unsupported, Error, Result};
use crate::fs::Volume;
use crate::obj::{ObjHeader, OBJECT_TYPE_CHECKPOINT_MAP, OBJECT_TYPE_FS, OBJECT_TYPE_NX_SUPERBLOCK};
use crate::omap::Omap;

pub const NX_MAGIC: u32 = 0x4253_584e; // "NXSB"
pub const APFS_MAGIC: u32 = 0x4253_5041; // "APSB"
const NX_MAX_FILE_SYSTEMS: usize = 100;
const CHECKPOINT_MAP_LAST: u32 = 0x1;
/// Cached B-tree nodes (32 MiB at 4 KiB blocks). Safe while read-only:
/// APFS never rewrites a block in place, so a cached node cannot go stale.
const NODE_CACHE_CAP: usize = 8192;

pub const NX_INCOMPAT_VERSION1: u64 = 0x1;
pub const NX_INCOMPAT_FUSION: u64 = 0x100;

pub const APFS_FS_UNENCRYPTED: u64 = 0x1;
pub const APFS_INCOMPAT_CASE_INSENSITIVE: u64 = 0x1;
pub const APFS_INCOMPAT_NORMALIZATION_INSENSITIVE: u64 = 0x8;
pub const APFS_INCOMPAT_SEALED_VOLUME: u64 = 0x20;

/// Block-level reads shared by every layer.
pub struct Reader {
    dev: Box<dyn BlockDevice>,
    pub block_size: u32,
    pub block_count: u64,
    nodes: Mutex<HashMap<u64, Arc<Node>>>,
    node_reads: AtomicU64,
}

impl Reader {
    fn new(dev: Box<dyn BlockDevice>, block_size: u32, block_count: u64) -> Self {
        Self { dev, block_size, block_count, nodes: Mutex::new(HashMap::new()), node_reads: AtomicU64::new(0) }
    }

    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.dev.read_at(off, buf)
    }

    pub fn read_block(&self, paddr: u64) -> Result<Vec<u8>> {
        if paddr == 0 || paddr >= self.block_count {
            return Err(corrupt(format!("block address {paddr:#x} outside the container")));
        }
        let mut b = vec![0u8; self.block_size as usize];
        self.dev.read_at(paddr * self.block_size as u64, &mut b)?;
        Ok(b)
    }

    /// Reads a block and verifies its object checksum.
    pub fn read_object(&self, paddr: u64) -> Result<Vec<u8>> {
        let b = self.read_block(paddr)?;
        if !checksum::verify(&b) {
            return Err(corrupt(format!("checksum mismatch in object at block {paddr:#x}")));
        }
        Ok(b)
    }
}

impl NodeSource for Reader {
    fn read_node(&self, paddr: u64) -> Result<Arc<Node>> {
        if let Some(n) = self.nodes.lock().unwrap().get(&paddr) {
            return Ok(Arc::clone(n));
        }
        let node = Arc::new(Node::parse(self.read_object(paddr)?)?);
        self.node_reads.fetch_add(1, Ordering::Relaxed);
        let mut cache = self.nodes.lock().unwrap();
        if cache.len() >= NODE_CACHE_CAP {
            cache.clear(); // crude, but the hot upper levels refill within a few lookups
        }
        cache.insert(paddr, Arc::clone(&node));
        Ok(node)
    }
}

#[derive(Debug, Clone)]
pub struct NxSuperblock {
    pub hdr: ObjHeader,
    pub block_size: u32,
    pub block_count: u64,
    pub features: u64,
    pub ro_compat_features: u64,
    pub incompat_features: u64,
    pub uuid: [u8; 16],
    pub next_oid: u64,
    pub next_xid: u64,
    pub xp_desc_blocks: u32,
    pub xp_data_blocks: u32,
    pub xp_desc_base: u64,
    pub xp_data_base: u64,
    pub xp_desc_next: u32,
    pub xp_data_next: u32,
    pub xp_desc_index: u32,
    pub xp_desc_len: u32,
    pub xp_data_index: u32,
    pub xp_data_len: u32,
    pub spaceman_oid: u64,
    pub omap_oid: u64,
    pub reaper_oid: u64,
    pub max_file_systems: u32,
    pub fs_oids: Vec<u64>,
    pub flags: u64,
    pub ephemeral_info: [u64; 4],
    /// The whole block, so a rewrite keeps fields this crate does not model.
    pub raw: Vec<u8>,
}

impl NxSuperblock {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if u32_at(b, 32)? != NX_MAGIC {
            return Err(corrupt("missing NXSB magic"));
        }
        let max_fs = (u32_at(b, 180)? as usize).min(NX_MAX_FILE_SYSTEMS);
        let fs_oids = (0..max_fs).map(|i| u64_at(b, 184 + i * 8)).collect::<Result<Vec<_>>>()?;
        Ok(Self {
            hdr: ObjHeader::parse(b)?,
            block_size: u32_at(b, 36)?,
            block_count: u64_at(b, 40)?,
            features: u64_at(b, 48)?,
            ro_compat_features: u64_at(b, 56)?,
            incompat_features: u64_at(b, 64)?,
            uuid: uuid_at(b, 72)?,
            next_oid: u64_at(b, 88)?,
            next_xid: u64_at(b, 96)?,
            xp_desc_blocks: u32_at(b, 104)?,
            xp_data_blocks: u32_at(b, 108)?,
            xp_desc_base: u64_at(b, 112)?,
            xp_data_base: u64_at(b, 120)?,
            xp_desc_next: u32_at(b, 128)?,
            xp_data_next: u32_at(b, 132)?,
            xp_desc_index: u32_at(b, 136)?,
            xp_desc_len: u32_at(b, 140)?,
            xp_data_index: u32_at(b, 144)?,
            xp_data_len: u32_at(b, 148)?,
            spaceman_oid: u64_at(b, 152)?,
            omap_oid: u64_at(b, 160)?,
            reaper_oid: u64_at(b, 168)?,
            max_file_systems: u32_at(b, 180)?,
            fs_oids: fs_oids.into_iter().filter(|&o| o != 0).collect(),
            flags: u64_at(b, 1264)?,
            ephemeral_info: [u64_at(b, 1312)?, u64_at(b, 1320)?, u64_at(b, 1328)?, u64_at(b, 1336)?],
            raw: b.to_vec(),
        })
    }
}

/// Where the latest checkpoint stored one ephemeral object.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointMapping {
    pub obj_type: u32,
    pub subtype: u32,
    pub size: u32,
    pub fs_oid: u64,
    pub oid: u64,
    pub paddr: u64,
}

#[derive(Debug, Clone)]
pub struct ApfsSuperblock {
    pub hdr: ObjHeader,
    pub fs_index: u32,
    pub features: u64,
    pub ro_compat_features: u64,
    pub incompat_features: u64,
    pub unmount_time: u64,
    pub reserve_blocks: u64,
    pub quota_blocks: u64,
    pub alloc_blocks: u64,
    pub omap_oid: u64,
    pub root_tree_oid: u64,
    pub extentref_tree_oid: u64,
    pub snap_meta_tree_oid: u64,
    pub next_obj_id: u64,
    pub num_files: u64,
    pub num_directories: u64,
    pub num_symlinks: u64,
    pub num_other_fsobjects: u64,
    pub num_snapshots: u64,
    pub vol_uuid: [u8; 16],
    pub last_mod_time: u64,
    pub fs_flags: u64,
    pub formatted_by: String,
    pub last_modified_by: String,
    pub name: String,
    pub role: u16,
    /// The whole block, so a rewrite keeps fields this crate does not model.
    pub raw: Vec<u8>,
}

impl ApfsSuperblock {
    pub fn parse(b: &[u8]) -> Result<Self> {
        let hdr = ObjHeader::parse(b)?;
        if hdr.kind() != OBJECT_TYPE_FS || u32_at(b, 32)? != APFS_MAGIC {
            return Err(corrupt(format!("object {:#x} is not a volume superblock", hdr.oid)));
        }
        Ok(Self {
            hdr,
            fs_index: u32_at(b, 36)?,
            features: u64_at(b, 40)?,
            ro_compat_features: u64_at(b, 48)?,
            incompat_features: u64_at(b, 56)?,
            unmount_time: u64_at(b, 64)?,
            reserve_blocks: u64_at(b, 72)?,
            quota_blocks: u64_at(b, 80)?,
            alloc_blocks: u64_at(b, 88)?,
            omap_oid: u64_at(b, 128)?,
            root_tree_oid: u64_at(b, 136)?,
            extentref_tree_oid: u64_at(b, 144)?,
            snap_meta_tree_oid: u64_at(b, 152)?,
            next_obj_id: u64_at(b, 176)?,
            num_files: u64_at(b, 184)?,
            num_directories: u64_at(b, 192)?,
            num_symlinks: u64_at(b, 200)?,
            num_other_fsobjects: u64_at(b, 208)?,
            num_snapshots: u64_at(b, 216)?,
            vol_uuid: uuid_at(b, 240)?,
            last_mod_time: u64_at(b, 256)?,
            fs_flags: u64_at(b, 264)?,
            formatted_by: cstr(slice_at(b, 272, 32)?),
            last_modified_by: cstr(slice_at(b, 320, 32)?),
            name: cstr(slice_at(b, 704, 256)?),
            role: u16_at(b, 964)?,
            raw: b.to_vec(),
        })
    }

    pub fn is_encrypted(&self) -> bool {
        self.fs_flags & APFS_FS_UNENCRYPTED == 0
    }

    pub fn is_case_insensitive(&self) -> bool {
        self.incompat_features & APFS_INCOMPAT_CASE_INSENSITIVE != 0
    }

    pub fn is_sealed(&self) -> bool {
        self.incompat_features & APFS_INCOMPAT_SEALED_VOLUME != 0
    }

    pub fn role_name(&self) -> &'static str {
        match self.role {
            0x000 => "none",
            0x001 => "system",
            0x002 => "user",
            0x004 => "recovery",
            0x008 => "vm",
            0x010 => "preboot",
            0x020 => "installer",
            0x040 => "data",
            0x080 => "baseband",
            0x0c0 => "update",
            0x100 => "xart",
            0x140 => "hardware",
            0x180 => "backup",
            0x240 => "enterprise",
            0x2c0 => "prelogin",
            _ => "other",
        }
    }
}

pub struct Container {
    pub(crate) r: Reader,
    pub sb: NxSuperblock,
    pub checkpoint_map: Vec<CheckpointMapping>,
    pub(crate) omap: Omap,
}

impl Container {
    pub fn open(dev: Box<dyn BlockDevice>) -> Result<Self> {
        // Block 0 holds a (possibly stale) superblock copy; APFS blocks are at least 4 KiB.
        let mut b0 = vec![0u8; 4096];
        dev.read_at(0, &mut b0)?;
        if u32_at(&b0, 32)? != NX_MAGIC {
            return Err(Error::NotFound("APFS container superblock (NXSB) at block 0".into()));
        }
        let block_size = u32_at(&b0, 36)?;
        if !(4096..=65536).contains(&block_size) || !block_size.is_power_of_two() {
            return Err(corrupt(format!("block size {block_size}")));
        }
        if block_size != 4096 {
            b0.resize(block_size as usize, 0);
            dev.read_at(0, &mut b0)?;
        }
        let sb0 = NxSuperblock::parse(&b0)?;
        let r = Reader::new(dev, block_size, sb0.block_count);

        let (sb, checkpoint_map) = latest_checkpoint(&r, &sb0)?;
        if sb.incompat_features & NX_INCOMPAT_VERSION1 != 0 {
            return Err(unsupported("pre-release APFS version 1 containers"));
        }
        if sb.incompat_features & NX_INCOMPAT_FUSION != 0 {
            return Err(unsupported("Fusion Drive containers"));
        }
        let omap = Omap::load(&r, &r.read_object(sb.omap_oid)?)?;
        Ok(Self { r, sb, checkpoint_map, omap })
    }

    pub fn block_size(&self) -> u32 {
        self.r.block_size
    }

    /// Any block, unchecked (block 0 included). For inspection tools.
    pub fn raw_block(&self, paddr: u64) -> Result<Vec<u8>> {
        if paddr >= self.r.block_count {
            return Err(corrupt(format!("block address {paddr:#x} outside the container")));
        }
        let mut b = vec![0u8; self.r.block_size as usize];
        self.r.read_at(paddr * self.r.block_size as u64, &mut b)?;
        Ok(b)
    }

    /// Reads an ephemeral object of the mounted checkpoint (it may span
    /// several blocks of the checkpoint data ring) and verifies its checksum.
    pub fn ephemeral(&self, oid: u64) -> Result<Vec<u8>> {
        let m = self
            .checkpoint_map
            .iter()
            .find(|m| m.oid == oid)
            .ok_or_else(|| corrupt(format!("ephemeral object {oid:#x} is not in the checkpoint map")))?;
        let bs = self.r.block_size as u64;
        let blocks = (m.size as u64).div_ceil(bs);
        let ring = (self.sb.xp_data_blocks & 0x7fff_ffff) as u64;
        let mut out = Vec::with_capacity((blocks * bs) as usize);
        for i in 0..blocks {
            // The data area is a ring: an object may wrap from its end back to its start.
            let offset = (m.paddr - self.sb.xp_data_base + i) % ring.max(1);
            out.extend(self.raw_block(self.sb.xp_data_base + offset)?);
        }
        out.truncate(m.size as usize);
        if !checksum::verify(&out) {
            return Err(corrupt(format!("checksum mismatch in ephemeral object {oid:#x}")));
        }
        Ok(out)
    }

    /// Physical address of the newest version of a container-level virtual object.
    pub fn omap_lookup(&self, oid: u64) -> Result<u64> {
        self.omap
            .lookup(&self.r, oid, self.xid())?
            .map(|v| v.paddr)
            .ok_or_else(|| corrupt(format!("object {oid:#x} missing from the container object map")))
    }

    /// Physical address of an ephemeral object in the mounted checkpoint.
    pub fn ephemeral_paddr(&self, oid: u64) -> Option<u64> {
        self.checkpoint_map.iter().find(|m| m.oid == oid).map(|m| m.paddr)
    }

    pub fn space_manager(&self) -> Result<crate::spaceman::Spaceman> {
        crate::spaceman::Spaceman::parse(&self.ephemeral(self.sb.spaceman_oid)?)
    }

    /// The container object map's entries as (oid, xid, value).
    pub fn omap_entries(&self) -> Result<Vec<(u64, u64, crate::omap::OmapVal)>> {
        self.omap.entries(&self.r)
    }

    /// Reads and checksum-verifies a B-tree node.
    pub fn node(&self, paddr: u64) -> Result<Arc<Node>> {
        self.r.read_node(paddr)
    }

    /// B-tree nodes read from disk so far (cache misses).
    pub fn node_reads(&self) -> u64 {
        self.r.node_reads.load(Ordering::Relaxed)
    }

    /// Transaction id of the checkpoint we mounted.
    pub fn xid(&self) -> u64 {
        self.sb.hdr.xid
    }

    /// Superblocks of every volume, without opening their file trees.
    pub fn volume_superblocks(&self) -> Result<Vec<ApfsSuperblock>> {
        self.sb.fs_oids.iter().map(|&oid| self.volume_superblock(oid)).collect()
    }

    fn volume_superblock(&self, oid: u64) -> Result<ApfsSuperblock> {
        let v = self
            .omap
            .lookup(&self.r, oid, self.xid())?
            .ok_or_else(|| corrupt(format!("volume {oid:#x} missing from the container object map")))?;
        ApfsSuperblock::parse(&self.r.read_object(v.paddr)?)
    }

    /// Opens volume `index` (0-based, in container order) for reading.
    pub fn volume(&self, index: usize) -> Result<Volume<'_>> {
        let oid = *self
            .sb
            .fs_oids
            .get(index)
            .ok_or_else(|| Error::NotFound(format!("volume {index} (container has {})", self.sb.fs_oids.len())))?;
        Volume::open(self, self.volume_superblock(oid)?)
    }
}

/// Scans the checkpoint descriptor ring for the newest superblock whose
/// checkpoint maps are all intact.
fn latest_checkpoint(r: &Reader, sb0: &NxSuperblock) -> Result<(NxSuperblock, Vec<CheckpointMapping>)> {
    if sb0.xp_desc_blocks & 0x8000_0000 != 0 {
        return Err(unsupported("checkpoint descriptor area stored as a B-tree"));
    }
    let ring = sb0.xp_desc_blocks as u64;
    let mut candidates = Vec::new();
    for i in 0..ring {
        let b = r.read_block(sb0.xp_desc_base + i)?;
        let hdr = ObjHeader::parse(&b)?;
        if hdr.kind() == OBJECT_TYPE_NX_SUPERBLOCK && u32_at(&b, 32)? == NX_MAGIC && checksum::verify(&b) {
            candidates.push(NxSuperblock::parse(&b)?);
        }
    }
    candidates.sort_by_key(|s| Reverse(s.hdr.xid));
    for sb in candidates {
        if let Ok(map) = checkpoint_mappings(r, &sb, ring) {
            return Ok((sb, map));
        }
    }
    Err(corrupt("no intact checkpoint in the checkpoint descriptor area"))
}

fn checkpoint_mappings(r: &Reader, sb: &NxSuperblock, ring: u64) -> Result<Vec<CheckpointMapping>> {
    let mut out = Vec::new();
    // The checkpoint's descriptor blocks: its maps, then the superblock itself.
    for k in 0..sb.xp_desc_len.saturating_sub(1) as u64 {
        let paddr = sb.xp_desc_base + (sb.xp_desc_index as u64 + k) % ring;
        let b = r.read_object(paddr)?;
        let hdr = ObjHeader::parse(&b)?;
        if hdr.kind() != OBJECT_TYPE_CHECKPOINT_MAP || hdr.xid != sb.hdr.xid {
            return Err(corrupt(format!("block {paddr:#x} is not a checkpoint map of xid {}", sb.hdr.xid)));
        }
        let flags = u32_at(&b, 32)?;
        let count = u32_at(&b, 36)? as usize;
        for i in 0..count {
            let e = 40 + i * 40;
            out.push(CheckpointMapping {
                obj_type: u32_at(&b, e)?,
                subtype: u32_at(&b, e + 4)?,
                size: u32_at(&b, e + 8)?,
                fs_oid: u64_at(&b, e + 16)?,
                oid: u64_at(&b, e + 24)?,
                paddr: u64_at(&b, e + 32)?,
            });
        }
        if flags & CHECKPOINT_MAP_LAST != 0 {
            break;
        }
    }
    Ok(out)
}
