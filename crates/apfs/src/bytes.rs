//! Bounds-checked little-endian field readers. On-disk data is untrusted, so
//! a bad offset becomes an error instead of a panic.

use crate::error::{corrupt, Result};

pub(crate) fn slice_at(b: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    off.checked_add(len)
        .and_then(|end| b.get(off..end))
        .ok_or_else(|| corrupt(format!("{len} bytes at {off} past end of {}-byte buffer", b.len())))
}

pub(crate) fn u16_at(b: &[u8], off: usize) -> Result<u16> {
    slice_at(b, off, 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}

pub(crate) fn u32_at(b: &[u8], off: usize) -> Result<u32> {
    slice_at(b, off, 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}

pub(crate) fn u64_at(b: &[u8], off: usize) -> Result<u64> {
    slice_at(b, off, 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

pub(crate) fn uuid_at(b: &[u8], off: usize) -> Result<[u8; 16]> {
    slice_at(b, off, 16).map(|s| s.try_into().unwrap())
}

/// Decodes a NUL-terminated (or NUL-padded) UTF-8 field.
pub(crate) fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

pub fn format_uuid(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02X}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}
