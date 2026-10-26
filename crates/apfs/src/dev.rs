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

pub trait WriteDevice: BlockDevice {
    fn write_at(&self, off: u64, buf: &[u8]) -> Result<()>;
    /// Makes everything written so far durable.
    fn flush(&self) -> Result<()>;
}

/// A disk image file opened for writing. Raw disks, volumes and every other
/// device path are refused: until write support has been proven on images,
/// nothing in this crate may write to a real disk.
pub struct ImageFile {
    file: File,
    base: u64,
}

impl ImageFile {
    pub fn open_rw(path: &Path) -> Result<Self> {
        let s = path.as_os_str().to_string_lossy();
        if s.starts_with(r"\\") || s.starts_with("//") {
            return Err(crate::error::unsupported("writing to devices; only disk image files can be written"));
        }
        if !std::fs::metadata(path)?.is_file() {
            return Err(crate::error::unsupported("writing to anything but a regular disk image file"));
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { file, base: 0 })
    }

    /// The same file, addressed from `base` (e.g. the start of a partition).
    pub fn with_base(&self, base: u64) -> Result<Self> {
        Ok(Self { file: self.file.try_clone()?, base })
    }
}

impl BlockDevice for ImageFile {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        pread_exact(&self.file, self.base + off, buf)
    }
}

impl WriteDevice for ImageFile {
    fn write_at(&self, off: u64, buf: &[u8]) -> Result<()> {
        pwrite_all(&self.file, self.base + off, buf)
    }

    fn flush(&self) -> Result<()> {
        Ok(self.file.sync_data()?)
    }
}

#[cfg(windows)]
fn pwrite_all(file: &File, mut off: u64, mut buf: &[u8]) -> Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, off)?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into());
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn pwrite_all(file: &File, off: u64, buf: &[u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    Ok(file.write_all_at(buf, off)?)
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
