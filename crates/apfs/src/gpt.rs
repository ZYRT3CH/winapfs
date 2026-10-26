//! Minimal GPT reader, enough to find APFS partitions on a disk or disk image.

use crate::bytes::{slice_at, u32_at, u64_at, uuid_at};
use crate::dev::BlockDevice;
use crate::error::{corrupt, Result};

/// 7C3457EF-0000-11AA-AA11-00306543ECAC in GPT's mixed-endian byte order.
pub const APFS_PARTITION_TYPE: [u8; 16] = [
    0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
];

#[derive(Debug, Clone)]
pub struct Partition {
    /// 1-based, matching Windows' partition numbering.
    pub number: u32,
    pub type_guid: [u8; 16],
    pub name: String,
    pub offset: u64,
    pub size: u64,
}

impl Partition {
    pub fn is_apfs(&self) -> bool {
        self.type_guid == APFS_PARTITION_TYPE
    }
}

/// Returns `None` when the device has no GPT (e.g. a bare container image).
pub fn read_partitions(dev: &dyn BlockDevice) -> Result<Option<Vec<Partition>>> {
    // 4096 bytes covers LBA 1 on 512-byte-sector disks and stays aligned on 4Kn disks.
    let mut head = vec![0u8; 4096];
    dev.read_at(0, &mut head)?;
    let (sector, header) = if &head[512..520] == b"EFI PART" {
        (512u64, head[512..1024].to_vec())
    } else {
        let mut lba1 = vec![0u8; 4096];
        if dev.read_at(4096, &mut lba1).is_err() || &lba1[0..8] != b"EFI PART" {
            return Ok(None);
        }
        (4096u64, lba1)
    };

    let entries_lba = u64_at(&header, 72)?;
    let count = u32_at(&header, 80)? as usize;
    let entry_size = u32_at(&header, 84)? as usize;
    if entry_size < 128 || count > 4096 {
        return Err(corrupt(format!("GPT header: {count} entries of {entry_size} bytes")));
    }
    let len = (count * entry_size).div_ceil(sector as usize) * sector as usize;
    let mut table = vec![0u8; len];
    dev.read_at(entries_lba * sector, &mut table)?;

    let mut parts = Vec::new();
    for i in 0..count {
        let e = slice_at(&table, i * entry_size, entry_size)?;
        let type_guid = uuid_at(e, 0)?;
        if type_guid == [0; 16] {
            continue;
        }
        let first = u64_at(e, 32)?;
        let last = u64_at(e, 40)?;
        let name_units: Vec<u16> = slice_at(e, 56, 72)?
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        parts.push(Partition {
            number: i as u32 + 1,
            type_guid,
            name: String::from_utf16_lossy(&name_units),
            offset: first * sector,
            size: (last.saturating_sub(first) + 1) * sector,
        });
    }
    Ok(Some(parts))
}
