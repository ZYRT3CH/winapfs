//! Byte-addressed, read-only access to whatever holds the container: an image
//! file or a raw disk such as `\\.\PhysicalDrive3`.

use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::error::Result;

pub trait BlockDevice: Send + Sync {
    /// Fills `buf` from byte offset `off`. Raw Windows disks require `off` and
    /// `buf.len()` to be multiples of the sector size; APFS block reads always are.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()>;
}

pub struct FileDevice {
    file: File,
    base: u64,
}

impl FileDevice {
    /// Opens `path` read-only. The OS enforces this: the handle has no write
    /// access, so no code path can modify the disk through it.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self { file, base: 0 })
    }

    /// The same file, addressed from `base` (e.g. the start of a partition).
    pub fn with_base(&self, base: u64) -> Result<Self> {
        Ok(Self { file: self.file.try_clone()?, base })
    }
}

impl BlockDevice for FileDevice {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        pread_exact(&self.file, self.base + off, buf)
    }
}

#[cfg(windows)]
fn pread_exact(file: &File, mut off: u64, mut buf: &mut [u8]) -> Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, off)?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn pread_exact(file: &File, off: u64, buf: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    Ok(file.read_exact_at(buf, off)?)
}

/// In-memory device, used by tests.
impl BlockDevice for Vec<u8> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        let src = usize::try_from(off)
            .ok()
            .and_then(|o| self.get(o..o.checked_add(buf.len())?))
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(src);
        Ok(())
    }
}
