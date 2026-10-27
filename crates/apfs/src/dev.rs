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

    /// Another handle to the same file and base.
    pub fn try_clone(&self) -> Result<Self> {
        self.with_base(self.base)
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

static SIM_WRITE_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
static SIM_FLUSH_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// For performance tests on image files, which sit on fast local disks: the
/// milliseconds in environment variable `var` are added to every write or
/// flush, to act like a slow USB drive. Never applied to real disks.
fn sim_delay(cell: &std::sync::OnceLock<u64>, var: &str) {
    let ms = *cell.get_or_init(|| std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(0));
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
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

/// A raw disk (`\\.\PhysicalDriveN`) opened for writing, addressed from the
/// start of a partition. Only [`crate::write::Writer::open_real_disk`]
/// creates one, after checking the container's identity.
pub struct DiskFile {
    file: File,
    base: u64,
    /// An image file standing in for a disk: may act slow (see [`sim_delay`]).
    image: bool,
}

impl DiskFile {
    pub(crate) fn open_rw(path: &Path, base: u64) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let s = path.as_os_str().to_string_lossy();
        let image = !(s.starts_with(r"\\") || s.starts_with("//")) && std::fs::metadata(path)?.is_file();
        Ok(Self { file, base, image })
    }
}

impl BlockDevice for DiskFile {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        pread_exact(&self.file, self.base + off, buf)
    }
}

impl WriteDevice for DiskFile {
    fn write_at(&self, off: u64, buf: &[u8]) -> Result<()> {
        if self.image {
            sim_delay(&SIM_WRITE_MS, "WINAPFS_SIM_WRITE_MS");
        }
        pwrite_all(&self.file, self.base + off, buf)
    }

    fn flush(&self) -> Result<()> {
        if self.image {
            sim_delay(&SIM_FLUSH_MS, "WINAPFS_SIM_FLUSH_MS");
        }
        Ok(self.file.sync_all()?)
    }
}

/// A device whose writes are kept in memory: reads see them, the device
/// underneath never does. Runs the write code against a real disk without
/// changing it (a rehearsal).
pub struct Overlay {
    base: Box<dyn BlockDevice>,
    bs: u64,
    blocks: std::sync::Mutex<std::collections::HashMap<u64, Box<[u8]>>>,
}

impl Overlay {
    pub fn new(base: Box<dyn BlockDevice>, block_size: u64) -> Self {
        Self { base, bs: block_size, blocks: Default::default() }
    }

    /// How many blocks have been written.
    pub fn written(&self) -> usize {
        self.blocks.lock().unwrap().len()
    }
}

impl BlockDevice for Overlay {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        let bs = self.bs;
        let blocks = self.blocks.lock().unwrap();
        let end = off + buf.len() as u64;
        // Walk block by block, reading runs of unwritten blocks from the base in one go.
        let mut b = off / bs;
        while b * bs < end {
            let (s, e) = ((b * bs).max(off), ((b + 1) * bs).min(end));
            if let Some(data) = blocks.get(&b) {
                buf[(s - off) as usize..(e - off) as usize].copy_from_slice(&data[(s - b * bs) as usize..(e - b * bs) as usize]);
                b += 1;
                continue;
            }
            let mut last = b;
            while (last + 1) * bs < end && !blocks.contains_key(&(last + 1)) {
                last += 1;
            }
            let mut tmp = vec![0u8; ((last - b + 1) * bs) as usize];
            self.base.read_at(b * bs, &mut tmp)?;
            let e = ((last + 1) * bs).min(end);
            buf[(s - off) as usize..(e - off) as usize].copy_from_slice(&tmp[(s - b * bs) as usize..(e - b * bs) as usize]);
            b = last + 1;
        }
        Ok(())
    }
}

impl WriteDevice for Overlay {
    fn write_at(&self, off: u64, buf: &[u8]) -> Result<()> {
        let bs = self.bs;
        if off % bs != 0 || buf.len() as u64 % bs != 0 {
            return Err(crate::error::corrupt(format!("unaligned write at {off:#x}+{}", buf.len())));
        }
        let mut blocks = self.blocks.lock().unwrap();
        for (i, chunk) in buf.chunks(bs as usize).enumerate() {
            blocks.insert(off / bs + i as u64, chunk.into());
        }
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Lets several owners share one device.
pub struct Shared<T>(pub std::sync::Arc<T>);

impl<T: BlockDevice> BlockDevice for Shared<T> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_at(off, buf)
    }
}

impl<T: WriteDevice> WriteDevice for Shared<T> {
    fn write_at(&self, off: u64, buf: &[u8]) -> Result<()> {
        self.0.write_at(off, buf)
    }

    fn flush(&self) -> Result<()> {
        self.0.flush()
    }
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
