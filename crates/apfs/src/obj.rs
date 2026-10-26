//! The header every APFS object starts with (`obj_phys_t`) and object types.

use crate::bytes::{u32_at, u64_at};
use crate::error::Result;

pub const OBJ_HEADER_SIZE: usize = 32;

pub const OBJECT_TYPE_MASK: u32 = 0x0000_ffff;
pub const OBJ_STORAGETYPE_MASK: u32 = 0xc000_0000;
pub const OBJ_VIRTUAL: u32 = 0x0000_0000;
pub const OBJ_EPHEMERAL: u32 = 0x8000_0000;
pub const OBJ_PHYSICAL: u32 = 0x4000_0000;
pub const OBJ_NOHEADER: u32 = 0x2000_0000;
pub const OBJ_ENCRYPTED: u32 = 0x1000_0000;

pub const OBJECT_TYPE_NX_SUPERBLOCK: u32 = 0x01;
pub const OBJECT_TYPE_BTREE: u32 = 0x02;
pub const OBJECT_TYPE_BTREE_NODE: u32 = 0x03;
pub const OBJECT_TYPE_SPACEMAN: u32 = 0x05;
pub const OBJECT_TYPE_OMAP: u32 = 0x0b;
pub const OBJECT_TYPE_CHECKPOINT_MAP: u32 = 0x0c;
pub const OBJECT_TYPE_FS: u32 = 0x0d;
pub const OBJECT_TYPE_FSTREE: u32 = 0x0e;

#[derive(Debug, Clone, Copy)]
pub struct ObjHeader {
    pub cksum: u64,
    pub oid: u64,
    pub xid: u64,
    pub obj_type: u32,
    pub subtype: u32,
}

impl ObjHeader {
    pub fn parse(b: &[u8]) -> Result<Self> {
        Ok(Self {
            cksum: u64_at(b, 0)?,
            oid: u64_at(b, 8)?,
            xid: u64_at(b, 16)?,
            obj_type: u32_at(b, 24)?,
            subtype: u32_at(b, 28)?,
        })
    }

    pub fn kind(&self) -> u32 {
        self.obj_type & OBJECT_TYPE_MASK
    }
}
