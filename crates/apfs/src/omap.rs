//! Object maps (`omap_phys_t`): translate a virtual object id plus a
//! transaction id into the physical block holding that version of the object.

use std::sync::Arc;

use crate::btree::{self, BTreeInfo, Node, NodeSource, BTREE_PHYSICAL};
use crate::bytes::{u32_at, u64_at};
use crate::error::{corrupt, unsupported, Result};
use crate::obj::{ObjHeader, OBJ_PHYSICAL, OBJ_STORAGETYPE_MASK, OBJECT_TYPE_OMAP};

pub const OMAP_VAL_DELETED: u32 = 0x1;
pub const OMAP_VAL_ENCRYPTED: u32 = 0x4;
pub const OMAP_VAL_NOHEADER: u32 = 0x8;

#[derive(Debug, Clone, Copy)]
pub struct OmapVal {
    pub flags: u32,
    pub size: u32,
    pub paddr: u64,
}

pub struct Omap {
    pub flags: u32,
    pub snap_count: u32,
    root: Arc<Node>,
    info: BTreeInfo,
}

impl Omap {
    /// `object` is the raw, checksum-verified `omap_phys_t` block.
    pub fn load(src: &dyn NodeSource, object: &[u8]) -> Result<Self> {
        let hdr = ObjHeader::parse(object)?;
        if hdr.kind() != OBJECT_TYPE_OMAP {
            return Err(corrupt(format!("object {:#x} is type {:#x}, expected an object map", hdr.oid, hdr.kind())));
        }
        let tree_type = u32_at(object, 40)?;
        if tree_type & OBJ_STORAGETYPE_MASK != OBJ_PHYSICAL {
            return Err(unsupported(format!("object map tree with storage type {tree_type:#x}")));
        }
        let root = src.read_node(u64_at(object, 48)?)?;
        let info = root.info()?;
        if info.flags & BTREE_PHYSICAL == 0 || info.key_size != 16 || info.val_size != 16 {
            return Err(corrupt("object map B-tree has unexpected key/value layout"));
        }
        Ok(Self { flags: u32_at(object, 32)?, snap_count: u32_at(object, 36)?, root, info })
    }

    /// Every mapping as (oid, xid, value), in key order.
    pub fn entries(&self, src: &dyn NodeSource) -> Result<Vec<(u64, u64, OmapVal)>> {
        let mut out = Vec::new();
        btree::scan(src, &self.root, &self.info, &|p| Ok(p), &|_| Ok(std::cmp::Ordering::Equal), &mut |k, v| {
            out.push((
                u64_at(k, 0)?,
                u64_at(k, 8)?,
                OmapVal { flags: u32_at(v, 0)?, size: u32_at(v, 4)?, paddr: u64_at(v, 8)? },
            ));
            Ok(true)
        })?;
        Ok(out)
    }

    pub fn tree_root(&self) -> &Node {
        &self.root
    }

    /// The newest mapping of `oid` at or before transaction `xid`.
    pub fn lookup(&self, src: &dyn NodeSource, oid: u64, xid: u64) -> Result<Option<OmapVal>> {
        let target = (oid, xid);
        let hit = btree::lookup_le(src, &self.root, &self.info, &|p| Ok(p), &|k| {
            Ok((u64_at(k, 0)?, u64_at(k, 8)?).cmp(&target))
        })?;
        let Some((k, Some(v))) = hit else { return Ok(None) };
        if u64_at(&k, 0)? != oid {
            return Ok(None);
        }
        let val = OmapVal { flags: u32_at(&v, 0)?, size: u32_at(&v, 4)?, paddr: u64_at(&v, 8)? };
        Ok((val.flags & OMAP_VAL_DELETED == 0).then_some(val))
    }
}
