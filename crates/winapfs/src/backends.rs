//! Data sources for the WinFsp mount: an APFS volume, and a fixed in-memory
//! tree for testing the Windows side without a Mac drive.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// When the last change came in, and when the oldest uncommitted one did.
    activity: Mutex<(Instant, Option<Instant>)>,
    /// Changes Windows had been told were made could not be saved. From then
    /// on every change is refused, so programs see errors (and a move stops
    /// before deleting more originals) instead of losing more.
    failed: AtomicBool,
    /// Committed data read ahead; Windows does not cache writable mounts.
    window: Mutex<ReadWindow>,
    /// Counts changes: a window read before the latest one is not used.
    generation: AtomicU64,
    /// Free bytes as of a transaction: (xid, bytes).
    free: Mutex<(u64, u64)>,
}

/// Free bytes in the committed state, with the transaction they are from.
fn free_space(s: &Session, total: u64, block_size: u64) -> (u64, u64) {
    let used: u64 = s.container().volume_superblocks().map_or(0, |v| v.iter().map(|v| v.alloc_blocks).sum());
    (s.container().xid(), total.saturating_sub(used * block_size))
}

#[derive(Default)]
struct ReadWindow {
    id: u64,
    generation: u64,
    start: u64,
    data: Vec<u8>,
    next_fetch: usize,
}

/// Smallest and largest read-ahead: reads arrive in small pieces, and on a
/// USB link every one costs a round trip, so sequential readers get ever
/// larger fetches.
const READ_AHEAD_MIN: usize = 256 * 1024;
const READ_AHEAD_MAX: usize = 8 * 1024 * 1024;

/// Changes are committed once the volume has been quiet this long...
const COMMIT_AFTER_QUIET: Duration = Duration::from_millis(1000);
/// ...or once the oldest uncommitted change is this old.
const COMMIT_AT_MOST_EVERY: Duration = Duration::from_secs(5);

impl ApfsRwBackend {
    /// Serves a session already open: on an image, a real disk, or an
    /// overlay. Changes collect in one transaction and are committed by
    /// [`ApfsRwBackend::start_committer`], on flush and at unmount.
    pub fn from_session(mut s: Session) -> apfs::Result<Self> {
        s.set_batch(true);
        let v = s.volume()?;
        let block_size = s.container().block_size() as u64;
        let (label, rule, serial, created) =
            (v.sb.name.clone(), v.name_rule(), u32::from_le_bytes(v.sb.vol_uuid[..4].try_into().unwrap()), v.sb.last_mod_time);
        drop(v);
        let total = s.container().sb.block_count * block_size;
        let free = free_space(&s, total, block_size);
        Ok(Self {
            free: Mutex::new(free),
            s: Mutex::new(s),
            label,
            rule,
            serial,
            created,
            block_size,
            total,
            decompressed: Mutex::new(None),
            activity: Mutex::new((Instant::now(), None)),
            failed: AtomicBool::new(false),
            window: Mutex::new(ReadWindow::default()),
            generation: AtomicU64::new(0),
        })
    }

    pub fn session(&self) -> std::sync::MutexGuard<'_, Session> {
        self.s.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Runs a change on the session, unless changes were lost before; a
    /// loss this one causes stops all further changes.
    fn change<R>(&self, s: &mut Session, f: impl FnOnce(&mut Session) -> apfs::Result<R>) -> Result<R, NTSTATUS> {
        if self.failed.load(Ordering::SeqCst) {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        let r = f(s);
        self.check_lost(s);
        let r = r.map_err(status)?;
        self.changed();
        Ok(r)
    }

    fn check_lost(&self, s: &mut Session) {
        if let Some(why) = s.take_lost() {
            self.failed.store(true, Ordering::SeqCst);
            crate::trace::log(&format!(
                "winapfs: changes from the last few seconds could not be saved ({why}). \
                 The drive now refuses changes to keep the rest safe: unmount it, plug it back in, \
                 and check the files you copied most recently."
            ));
        }
    }

    /// Reads committed data through a window that grows while reads stay
    /// sequential. Runs under the session lock, so no change slips in.
    fn read_ahead(&self, v: &Volume, inode: &Inode, off: u64, buf: &mut [u8]) -> Result<usize, NTSTATUS> {
        let size = v.file_size(inode).map_err(status)?;
        if off >= size {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(size - off) as usize;
        let generation = self.generation.load(Ordering::SeqCst);
        let mut w = self.window.lock().unwrap();
        let same = w.id == inode.id && w.generation == generation;
        let end = w.start + w.data.len() as u64;
        if same && off >= w.start && off + want as u64 <= end {
            let s = (off - w.start) as usize;
            buf[..want].copy_from_slice(&w.data[s..s + want]);
            return Ok(want);
        }
        let sequential = same && off >= w.start && off <= end;
        let next_fetch = if sequential { (w.next_fetch * 2).clamp(READ_AHEAD_MIN, READ_AHEAD_MAX) } else { READ_AHEAD_MIN };
        let fetch = (next_fetch.max(want) as u64).min(size - off) as usize;
        let mut data = vec![0u8; fetch];
        let n = v.read_at(inode, off, &mut data).map_err(status)?;
        data.truncate(n);
        let k = want.min(n);
        buf[..k].copy_from_slice(&data[..k]);
        *w = ReadWindow { id: inode.id, generation, start: off, data, next_fetch };
        Ok(k)
    }

    /// Records a change: the committer waits for a quiet moment.
    fn changed(&self) {
        let now = Instant::now();
        let mut a = self.activity.lock().unwrap();
        a.0 = now;
        a.1.get_or_insert(now);
    }

    /// Starts the thread that commits collected changes.
    pub fn start_committer(&'static self) {
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(250));
            let due = {
                let a = self.activity.lock().unwrap();
                a.1.is_some_and(|oldest| a.0.elapsed() >= COMMIT_AFTER_QUIET || oldest.elapsed() >= COMMIT_AT_MOST_EVERY)
            };
            if due {
                let _ = Backend::flush(self);
            }
        });
    }
}

impl Backend for ApfsRwBackend {
    fn label(&self) -> String {
        self.label.clone()
    }

    /// Explorer asks for this constantly while copying: answer from the last
    /// count unless a commit has changed it, and never wait behind a commit.
    fn total_and_free(&self) -> (u64, u64) {
        let _t = Timer("volume info".to_string(), Instant::now());
        let mut free = self.free.lock().unwrap();
        if let Ok(s) = self.s.try_lock() {
            if s.container().xid() != free.0 {
                *free = free_space(&s, self.total, self.block_size);
            }
        }
        (self.total, free.1)
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
        let start = Instant::now();
        let mut s = self.session();
        let _t = Timer(format!("list {dir}"), start);
        let entries = s.list(dir).map_err(status)?;
        let v = s.volume().map_err(status)?;
        Ok(entries.into_iter().map(|(name, inode)| (name, meta_of(&v, &inode))).collect())
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
        let start = Instant::now();
        let mut s = self.session();
        let _t = Timer(format!("read {} bytes at {off} of {id}", buf.len()), start);
        if let Some(n) = s.read_pending(id, off, buf).map_err(status)? {
            return Ok(n);
        }
        let xid = s.container().xid();
        let v = s.volume().map_err(status)?;
        let inode = v.inode(id).map_err(status)?;
        if !inode.is_compressed() {
            return self.read_ahead(&v, &inode, off, buf);
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
        let start = Instant::now();
        let mut s = self.session();
        let _t = Timer(format!("stat {id}"), start);
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
        let _t = Timer(format!("create {name:?} in {dir}"), Instant::now());
        let kind = if is_dir { NewKind::Folder } else { NewKind::File };
        let id = self.change(&mut self.session(), |s| s.create(dir, name, kind))?;
        self.stat(id)
    }

    fn write(&self, id: u64, off: u64, data: &[u8]) -> Result<u64, NTSTATUS> {
        let start = Instant::now();
        let mut s = self.session();
        let _t = Timer(format!("write {} bytes at {off} of {id}", data.len()), start);
        self.change(&mut s, |s| s.write(id, off, data))?;
        Ok(s.pending_size(id).unwrap_or(off + data.len() as u64))
    }

    fn set_size(&self, id: u64, size: u64) -> Result<(), NTSTATUS> {
        let _t = Timer(format!("set size of {id} to {size}"), Instant::now());
        self.change(&mut self.session(), |s| s.set_size(id, size))
    }

    fn remove(&self, dir: u64, name: &str) -> Result<(), NTSTATUS> {
        let _t = Timer(format!("remove {name:?} from {dir}"), Instant::now());
        self.change(&mut self.session(), |s| s.remove(dir, name))
    }

    fn rename(&self, sdir: u64, sname: &str, ddir: u64, dname: &str, replace: bool) -> Result<(), NTSTATUS> {
        let _t = Timer(format!("rename {sname:?}"), Instant::now());
        self.change(&mut self.session(), |s| s.rename(sdir, sname, ddir, dname, replace))
    }

    fn set_info(&self, id: u64, t: SetTimes, hidden: Option<bool>) -> Result<(), NTSTATUS> {
        let _t = Timer(format!("set info of {id}"), Instant::now());
        self.change(&mut self.session(), |s| {
            if t.create.is_some() || t.access.is_some() || t.write.is_some() || t.change.is_some() {
                s.set_times(id, Times { create: t.create, modify: t.write, change: t.change, access: t.access })?;
            }
            if let Some(h) = hidden {
                s.set_hidden(id, h)?;
            }
            Ok(())
        })
    }

    /// Commits everything collected so far.
    fn flush(&self) -> Result<(), NTSTATUS> {
        let start = Instant::now();
        let mut s = self.session();
        let _t = Timer("flush".to_string(), start);
        // Taken under the session lock, so changes made meanwhile wait for the next commit.
        self.activity.lock().unwrap().1 = None;
        let r = s.flush();
        self.check_lost(&mut s);
        r.map_err(status)
    }

    /// A file written through this mount was closed: the committer saves it
    /// at the next quiet moment.
    fn file_closed(&self) -> Result<(), NTSTATUS> {
        self.changed();
        Ok(())
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

/// Reports an operation's duration when dropped, if slow (see `trace`).
struct Timer(String, Instant);

impl Drop for Timer {
    fn drop(&mut self) {
        crate::trace::slow(&self.0, self.1);
    }
}
