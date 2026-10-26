//! Data sources for the WinFsp mount: an APFS volume, and a fixed in-memory
//! tree for testing the Windows side without a Mac drive.

use apfs::{Container, Extent, FileKind, Inode, Volume};

use crate::mountfs::{Backend, FileReader, Meta};
use crate::winfsp::*;

const UF_HIDDEN: u32 = 0x8000;

/// Maps a library error to an NTSTATUS, logging anything but "not found"
/// (which Windows probes for constantly).
fn status(e: apfs::Error) -> NTSTATUS {
    match e {
        apfs::Error::NotFound(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        other => {
            eprintln!("winapfs: {other}");
            match other {
                apfs::Error::Corrupt(_) => STATUS_FILE_CORRUPT_ERROR,
                apfs::Error::Unsupported(_) => STATUS_NOT_SUPPORTED,
                _ => STATUS_IO_DEVICE_ERROR,
            }
        }
    }
}

pub struct ApfsBackend {
    vol: Volume<'static>,
    block_size: u64,
    total: u64,
    free: u64,
}

impl ApfsBackend {
    pub fn new(c: &'static Container, index: usize) -> apfs::Result<Self> {
        let vol = c.volume(index)?;
        let block_size = c.block_size() as u64;
        let total = c.sb.block_count * block_size;
        let used: u64 = c.volume_superblocks()?.iter().map(|v| v.alloc_blocks * block_size).sum();
        Ok(Self { vol, block_size, total, free: total.saturating_sub(used) })
    }

    fn meta(&self, inode: &Inode) -> Meta {
        let is_dir = inode.kind() == FileKind::Directory;
        let size = match self.vol.file_size(inode) {
            _ if is_dir => 0,
            Ok(size) => size,
            Err(e) => {
                status(e); // logged; shown as empty rather than hiding the file
                0
            }
        };
        Meta {
            id: inode.id,
            is_dir,
            size,
            hidden: inode.bsd_flags & UF_HIDDEN != 0,
            create: inode.create_time,
            access: inode.access_time,
            write: inode.mod_time,
            change: inode.change_time,
        }
    }
}

impl Backend for ApfsBackend {
    fn label(&self) -> String {
        self.vol.sb.name.clone()
    }

    fn total_and_free(&self) -> (u64, u64) {
        (self.total, self.free)
    }

    fn case_insensitive(&self) -> bool {
        self.vol.sb.is_case_insensitive()
    }

    fn serial(&self) -> u32 {
        u32::from_le_bytes(self.vol.sb.vol_uuid[..4].try_into().unwrap())
    }

    fn block_size(&self) -> u64 {
        self.block_size
    }

    fn created(&self) -> u64 {
        self.vol.sb.last_mod_time
    }

    fn root(&self) -> Result<Meta, NTSTATUS> {
        self.vol.root().map(|i| self.meta(&i)).map_err(status)
    }

    fn list(&self, dir: u64) -> Result<Vec<(String, Meta)>, NTSTATUS> {
        let entries = self.vol.read_dir(dir).map_err(status)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            match self.vol.inode(e.file_id) {
                Ok(inode) => out.push((e.name, self.meta(&inode))),
                Err(err) => {
                    status(err); // logged; the entry is left out rather than failing the folder
                }
            }
        }
        Ok(out)
    }

    fn name_key(&self, name: &str) -> String {
        self.vol.name_key(name)
    }

    fn open(&'static self, file: &Meta) -> Result<Box<dyn FileReader>, NTSTATUS> {
        let inode = self.vol.inode(file.id).map_err(status)?;
        if inode.is_compressed() {
            // Compressed files are small in practice; decompress once per open handle.
            let mut data = Vec::new();
            self.vol.read_file(&inode, &mut data).map_err(status)?;
            return Ok(Box::new(MemReader(data)));
        }
        let extents = self.vol.file_extents(&inode).map_err(status)?;
        Ok(Box::new(ExtentReader { vol: &self.vol, inode, extents }))
    }
}

struct ExtentReader {
    vol: &'static Volume<'static>,
    inode: Inode,
    extents: Vec<Extent>,
}

impl FileReader for ExtentReader {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, NTSTATUS> {
        self.vol.read_extents_at(&self.inode, &self.extents, off, buf).map_err(status)
    }
}

struct MemReader(Vec<u8>);

impl FileReader for MemReader {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, NTSTATUS> {
        let Some(rest) = usize::try_from(off).ok().and_then(|o| self.0.get(o..)) else { return Ok(0) };
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }
}

/// Byte `i` of the demo tree's `big.bin`; tests recompute it to check reads.
pub fn demo_byte(i: u64) -> u8 {
    (i.wrapping_mul(31) ^ (i >> 9)) as u8
}

pub const DEMO_BIG_LEN: u64 = 10 * 1024 * 1024 + 123;
pub const DEMO_MANY: usize = 2000;

struct DemoNode {
    name: String,
    parent: u64,
    is_dir: bool,
    data: Vec<u8>,
}

/// A fixed tree exercising what real Mac drives contain: names Windows
/// rejects, non-ASCII names, hidden files, and a file larger than one read.
pub struct DemoBackend {
    nodes: Vec<DemoNode>, // node id = index + 2, so the root is 2 as on APFS
}

impl DemoBackend {
    pub fn new() -> Self {
        let mut b = Self { nodes: Vec::new() };
        let root = b.add("", 1, true, Vec::new());
        b.add("hello.txt", root, false, b"Hello from winapfs\r\n".to_vec());
        b.add(".hidden", root, false, b"hidden\r\n".to_vec());
        let odd = b.add("Folder: with colon", root, true, Vec::new());
        b.add("notes \"quoted\".txt", odd, false, b"quoted\r\n".to_vec());
        b.add("trailing space ", odd, false, b"space\r\n".to_vec());
        b.add("big.bin", odd, false, (0..DEMO_BIG_LEN).map(demo_byte).collect());
        let nordic = b.add("Råfiler", root, true, Vec::new());
        b.add("æøå.txt", nordic, false, "blåbærsyltetøy\r\n".as_bytes().to_vec());
        // Enough entries that Windows must page through ReadDirectory with markers.
        let many = b.add("many", root, true, Vec::new());
        for i in 0..DEMO_MANY {
            b.add(&format!("file {i:04}.txt"), many, false, format!("{i}\r\n").into_bytes());
        }
        b
    }

    fn add(&mut self, name: &str, parent: u64, is_dir: bool, data: Vec<u8>) -> u64 {
        self.nodes.push(DemoNode { name: name.into(), parent, is_dir, data });
        self.nodes.len() as u64 + 1
    }

    fn meta(&self, id: u64) -> Meta {
        const T: u64 = 1_714_521_600 * 1_000_000_000; // 2024-05-01 00:00 UTC
        let n = &self.nodes[(id - 2) as usize];
        let size = n.data.len() as u64;
        Meta { id, is_dir: n.is_dir, size, hidden: false, create: T, access: T, write: T, change: T }
    }
}

impl Backend for DemoBackend {
    fn label(&self) -> String {
        "winapfs demo".into()
    }

    fn total_and_free(&self) -> (u64, u64) {
        (1 << 30, 0)
    }

    fn case_insensitive(&self) -> bool {
        true
    }

    fn serial(&self) -> u32 {
        0x5eed_f00d
    }

    fn block_size(&self) -> u64 {
        4096
    }

    fn created(&self) -> u64 {
        self.meta(2).create
    }

    fn root(&self) -> Result<Meta, NTSTATUS> {
        Ok(self.meta(2))
    }

    fn list(&self, dir: u64) -> Result<Vec<(String, Meta)>, NTSTATUS> {
        Ok(self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.parent == dir)
            .map(|(i, n)| (n.name.clone(), self.meta(i as u64 + 2)))
            .collect())
    }

    fn name_key(&self, name: &str) -> String {
        name.to_lowercase()
    }

    fn open(&'static self, file: &Meta) -> Result<Box<dyn FileReader>, NTSTATUS> {
        Ok(Box::new(MemReader(self.nodes[(file.id - 2) as usize].data.clone())))
    }
}
