//! Finding Mac (APFS) disks without administrator rights: any user may ask a
//! disk for its partition table (IOCTL_DISK_GET_DRIVE_LAYOUT_EX is a
//! FILE_ANY_ACCESS request), even though reading the disk needs admin.

use std::ffi::c_void;

use apfs::gpt::APFS_PARTITION_TYPE;

use crate::winfsp::wide;

const IOCTL_DISK_GET_DRIVE_LAYOUT_EX: u32 = 0x0007_0050;
const PARTITION_STYLE_GPT: u32 = 1;
/// `DRIVE_LAYOUT_INFORMATION_EX` header: style, count, then a 40-byte union.
const LAYOUT_HEADER: usize = 48;
/// `sizeof(PARTITION_INFORMATION_EX)` on 64-bit Windows.
const PARTITION_ENTRY: usize = 144;
/// Offset of `PartitionInformationGpt.PartitionType` within an entry.
const GPT_TYPE_OFFSET: usize = 32;
const MAX_DISKS: u32 = 32;

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn DeviceIoControl(
        device: *mut c_void,
        code: u32,
        input: *mut c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// Numbers of the physical disks that have an APFS partition, ascending.
pub fn apfs_disks() -> Vec<u32> {
    (0..MAX_DISKS).filter(|&n| has_apfs_partition(n) == Some(true)).collect()
}

/// `None` when the disk does not exist or will not answer.
fn has_apfs_partition(n: u32) -> Option<bool> {
    const FILE_SHARE_READ_WRITE: u32 = 0x3;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE: isize = -1;
    let path = wide(&format!(r"\\.\PhysicalDrive{n}"));
    // Desired access 0: no read or write rights, which is all a layout query needs.
    let h = unsafe {
        CreateFileW(path.as_ptr(), 0, FILE_SHARE_READ_WRITE, std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut())
    };
    if h as isize == INVALID_HANDLE {
        return None;
    }
    let mut buf = vec![0u64; (LAYOUT_HEADER + 128 * PARTITION_ENTRY) / 8];
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
            std::ptr::null_mut(),
            0,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), got as usize) };
    Some(layout_has_apfs(bytes))
}

fn layout_has_apfs(layout: &[u8]) -> bool {
    let u32_at = |o: usize| layout.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    if u32_at(0) != Some(PARTITION_STYLE_GPT) {
        return false;
    }
    let count = u32_at(4).unwrap_or(0) as usize;
    (0..count).any(|i| {
        let t = LAYOUT_HEADER + i * PARTITION_ENTRY + GPT_TYPE_OFFSET;
        layout.get(t..t + 16) == Some(&APFS_PARTITION_TYPE[..])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_gpt_layout() {
        let mut layout = vec![0u8; LAYOUT_HEADER + 2 * PARTITION_ENTRY];
        layout[0] = PARTITION_STYLE_GPT as u8;
        layout[4] = 2;
        assert!(!layout_has_apfs(&layout));
        let t = LAYOUT_HEADER + PARTITION_ENTRY + GPT_TYPE_OFFSET;
        layout[t..t + 16].copy_from_slice(&APFS_PARTITION_TYPE);
        assert!(layout_has_apfs(&layout));
        layout[0] = 0; // MBR disks never count
        assert!(!layout_has_apfs(&layout));
    }
}
