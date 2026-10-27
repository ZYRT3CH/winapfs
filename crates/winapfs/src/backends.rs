//! Data sources for the WinFsp mount: an APFS volume, and a fixed in-memory
//! tree for testing the Windows side without a Mac drive.

use std::path::Path;
use std::sync::{Arc, Mutex};

use apfs::write::{NewKind, Session, Times};
use apfs::{Container, Extent, FileKind, Inode, NameRule, Volume};

use crate::mountfs::{Backend, FileReader, Meta, SetTimes};
use crate::winfsp::*;

const UF_HIDDEN: u32 = 0x8000;

/// Maps a library error to an NTSTATUS, logging all but the expected ones
/// (Windows probes for missing names constantly).
fn status(e: apfs::Error) -> NTSTATUS {
    match e {
        apfs::Error::NotFound(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        apfs::Error::Exists(_) => STATUS_OBJECT_NAME_COLLISION,
        apfs::Error::NotEmpty(_) => STATUS_DIRECTORY_NOT_EMPTY,
        apfs::Error::InvalidName(_) => STATUS_OBJECT_NAME_INVALID,
        other => {
            eprintln!("winapfs: {other}");
            match other {
                apfs::Error::Corrupt(_) => STATUS_FILE_CORRUPT_ERROR,
                apfs::Error::Unsupported(_) => STATUS_NOT_SUPPORTED,
                apfs::Error::NoSpace(_) => STATUS_DISK_FULL,
                _ => STATUS_IO_DEVICE_ERROR,
            }
        }
    }
}

fn meta_of(v: &Volume, inode: &Inode) -> Meta {
    let is_dir = inode.kind() == FileKind::Directory;
    let size = match v.file_size(inode) {
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

/// A volume in a disk image, mounted read-write. EXPERIMENTAL.
pub struct ApfsRwBackend {
    s: Mutex<Session>,
    label: String,
    rule: NameRule,
    serial: u32,
    created: u64,
    block_size: u64,
    total: u64,
    /// The last compressed file read, decompressed: (inode, transaction, bytes).
    decompressed: Mutex<Option<(u64, u64, Arc<Vec<u8>>)>>,
}

impl ApfsRwBackend {
    pub fn new(image: &Path, vol: usize) -> apfs::Result<Self> {
        let s = Session::open(image, vol)?;
        let v = s.volume()?;
        let block_size = s.container().block_size() as u64;
        let (label, rule, serial, created) =
            (v.sb.name.clone(), v.name_rule(), u32::from_le_bytes(v.sb.vol_uuid[..4].try_into().unwrap()), v.sb.last_mod_time);
        drop(v);
        let total = s.container().sb.block_count * block_size;
        Ok(Self { s: Mutex::new(s), label, rule, serial, created, block_size, total, decompressed: Mutex::new(None) })
    }

    fn session(&self) -> std::sync::MutexGuard<'_, Session> {
        self.s.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Backend for ApfsRwBackend {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn total_and_free(&self) -> (u64, u64) {
        let s = self.session();
        let used: u64 = s.container().volume_superblocks().map_or(0, |v| v.iter().map(|v| v.alloc_blocks).sum());
        (self.total, self.total.saturating_sub(used * self.block_size))
    }

    fn case_insensitive(&self) -> bool {
        self.rule.case_insensitive
    }

    fn serial(&self) -> u32 {
        self.serial
    }

    fn block_size(&self) -> u64 {
        self.block_size
    }

    fn created(&self) -> u64 {
        self.created
    }

    fn root(&self) -> Result<Meta, NTSTATUS> {
        self.stat(2)
    }

    fn list(&self, dir: u64) -> Result<Vec<(String, Meta)>, NTSTATUS> {
        let s = self.session();
        let v = s.volume().map_err(status)?;
        let entries = v.read_dir(dir).map_err(status)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            match v.inode(e.file_id) {
                Ok(inode) => {
                    let mut m = meta_of(&v, &inode);
                    if let Some(size) = s.pending_size(m.id) {
                        m.size = size;
                    }
                    out.push((e.name, m));
                }
                Err(err) => {
                    status(err); // logged; the entry is left out rather than failing the folder
                }
            }
        }
        Ok(out)
    }

    fn name_key(&self, name: &str) -> String {
        self.rule.key(name)
    }

    fn open(&'static self, _file: &Meta) -> Result<Box<dyn FileReader>, NTSTATUS> {
        Err(STATUS_NOT_SUPPORTED) // reads go through `read`
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, id: u64, off: u64, buf: &mut [u8]) -> Result<usize, NTSTATUS> {
        let mut s = self.session();
        if let Some(n) = s.read_pending(id, off, buf).map_err(status)? {
            return Ok(n);
        }
        let xid = s.container().xid();
        let v = s.volume().map_err(status)?;
        let inode = v.inode(id).map_err(status)?;
        if !inode.is_compressed() {
            return v.read_at(&inode, off, buf).map_err(status);
        }
        let mut cache = self.decompressed.lock().unwrap();
        let data = match &*cache {
            Some((i, x, d)) if *i == id && *x == xid => Arc::clone(d),
            _ => {
                let mut d = Vec::new();
                v.read_file(&inode, &mut d).map_err(status)?;
                let d = Arc::new(d);
                *cache = Some((id, xid, Arc::clone(&d)));
                d
            }
        };
        let Some(rest) = usize::try_from(off).ok().and_then(|o| data.get(o..)) else { return Ok(0) };
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }

    fn stat(&self, id: u64) -> Result<Meta, NTSTATUS> {
        let mut s = self.session();
        let pending = s.pending_size(id);
        let inode = s.inode(id).map_err(status)?;
        let v = s.volume().map_err(status)?;
        let mut m = meta_of(&v, &inode);
        if let Some(size) = pending {
            m.size = size;
        }
        Ok(m)
    }

    fn create(&self, dir: u64, name: &str, is_dir: bool) -> Result<Meta, NTSTATUS> {
        let kind = if is_dir { NewKind::Folder } else { NewKind::File };
        let id = self.session().create(dir, name, kind).map_err(status)?;
        self.stat(id)
    }

    fn write(&self, id: u64, off: u64, data: &[u8]) -> Result<u64, NTSTATUS> {
        let mut s = self.session();
        s.write(id, off, data).map_err(status)?;
        Ok(s.pending_size(id).unwrap_or(off + data.len() as u64))
    }

    fn set_size(&self, id: u64, size: u64) -> Result<(), NTSTATUS> {
        self.session().set_size(id, size).map_err(status)
    }

    fn remove(&self, dir: u64, name: &str) -> Result<(), NTSTATUS> {
        self.session().remove(dir, name).map_err(status)
    }

    fn rename(&self, sdir: u64, sname: &str, ddir: u64, dname: &str, replace: bool) -> Result<(), NTSTATUS> {
        self.session().rename(sdir, sname, ddir, dname, replace).map_err(status)
    }

    fn set_info(&self, id: u64, t: SetTimes, hidden: Option<bool>) -> Result<(), NTSTATUS> {
        let mut s = self.session();
        if t.create.is_some() || t.access.is_some() || t.write.is_some() || t.change.is_some() {
            let times = Times { create: t.create, modify: t.write, change: t.change, access: t.access };
            s.set_times(id, times).map_err(status)?;
        }
        if let Some(h) = hidden {
            s.set_hidden(id, h).map_err(status)?;
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), NTSTATUS> {
        self.session().flush().map_err(status)
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
        meta_of(&self.vol, inode)
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
