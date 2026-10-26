//! A read-only WinFsp file system. Names, sizes and bytes come from a
//! [`Backend`]: an APFS volume, or a small in-memory tree that tests the
//! WinFsp side without a Mac drive.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{copy_nonoverlapping, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::winfsp::*;

const DIR_CACHE_CAP: usize = 4096;
const PATH_CACHE_CAP: usize = 200_000;
/// Longest file name we hand to Windows, in UTF-16 units (APFS allows 255 UTF-8 bytes).
const NAME_MAX: usize = 1024;

/// Everyone may read, list and run; SYSTEM and Administrators get full
/// control, which the read-only volume makes moot.
const SDDL: &str = "O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;WD)";

/// What Windows needs to know about a file. Times are nanoseconds since 1970.
#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub id: u64,
    pub is_dir: bool,
    pub size: u64,
    pub hidden: bool,
    pub create: u64,
    pub access: u64,
    pub write: u64,
    pub change: u64,
}

pub trait FileReader: Send + Sync {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, NTSTATUS>;
}

pub trait Backend: Send + Sync {
    fn label(&self) -> String;
    fn total_and_free(&self) -> (u64, u64);
    fn case_insensitive(&self) -> bool;
    fn serial(&self) -> u32;
    fn block_size(&self) -> u64;
    /// Nanoseconds since 1970.
    fn created(&self) -> u64;
    fn root(&self) -> Result<Meta, NTSTATUS>;
    /// The entries of a directory, with their Mac names.
    fn list(&self, dir: u64) -> Result<Vec<(String, Meta)>, NTSTATUS>;
    /// Two names are the same entry exactly when their keys are equal.
    fn name_key(&self, name: &str) -> String;
    fn open(&'static self, file: &Meta) -> Result<Box<dyn FileReader>, NTSTATUS>;
}

struct Child {
    win_name: Vec<u16>,
    meta: Meta,
    info: FileInfo,
}

struct Listing {
    /// Sorted by `win_name` (UTF-16 code units), the order ReadDirectory markers rely on.
    entries: Vec<Child>,
    by_key: HashMap<String, usize>,
}

struct Handle {
    meta: Meta,
    info: FileInfo,
    reader: OnceLock<Result<ReadAhead, NTSTATUS>>,
}

/// Smallest and largest fetch from the backend. Even through the Windows
/// cache, reads arrive in 4-8 KiB pieces, and on a USB link every request
/// costs a round trip, so sequential readers get ever larger fetches.
const READ_AHEAD_MIN: usize = 256 * 1024;
const READ_AHEAD_MAX: usize = 8 * 1024 * 1024;

/// Wraps a backend reader with a one-window cache that grows while reads
/// stay sequential and drops back to the minimum on a jump.
pub struct ReadAhead {
    inner: Box<dyn FileReader>,
    size: u64,
    window: Mutex<Window>,
}

struct Window {
    start: u64,
    data: Vec<u8>,
    next_fetch: usize,
}

impl ReadAhead {
    pub fn new(inner: Box<dyn FileReader>, size: u64) -> Self {
        Self { inner, size, window: Mutex::new(Window { start: 0, data: Vec::new(), next_fetch: READ_AHEAD_MIN / 2 }) }
    }
}

impl FileReader for ReadAhead {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, NTSTATUS> {
        if off >= self.size {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.size - off) as usize;
        let mut w = self.window.lock().unwrap();
        let end = w.start + w.data.len() as u64;
        if off >= w.start && off + want as u64 <= end {
            let s = (off - w.start) as usize;
            buf[..want].copy_from_slice(&w.data[s..s + want]);
            return Ok(want);
        }
        let sequential = off >= w.start && off <= end;
        w.next_fetch = if sequential { (w.next_fetch * 2).clamp(READ_AHEAD_MIN, READ_AHEAD_MAX) } else { READ_AHEAD_MIN };
        let fetch = (w.next_fetch.max(want) as u64).min(self.size - off) as usize;
        let mut data = vec![0u8; fetch];
        let n = self.inner.read_at(off, &mut data)?;
        data.truncate(n);
        let k = want.min(n);
        buf[..k].copy_from_slice(&data[..k]);
        w.start = off;
        w.data = data;
        Ok(k)
    }
}

struct Fs {
    backend: &'static dyn Backend,
    api: Api,
    security: Vec<u8>,
    label: Vec<u16>,
    root: (Meta, FileInfo),
    dirs: Mutex<HashMap<u64, Arc<Listing>>>,
    paths: Mutex<HashMap<String, (Meta, FileInfo)>>,
}

/// Characters a Mac name may hold but Windows rejects (reserved characters,
/// control characters, trailing spaces and dots) are shifted into the
/// private use area at U+F000, the convention of Cygwin and WinFsp's FUSE layer.
pub fn mac_to_win(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let keep = chars.iter().rposition(|&c| c != ' ' && c != '.').map_or(0, |i| i + 1);
    chars
        .iter()
        .enumerate()
        .map(|(i, &c)| if is_reserved(c) || i >= keep { char::from_u32(0xF000 + c as u32).unwrap() } else { c })
        .collect()
}

pub fn win_to_mac(name: &str) -> String {
    name.chars()
        .map(|c| match (c as u32).checked_sub(0xF000).and_then(char::from_u32) {
            Some(o) if (c as u32) < 0xF080 && (is_reserved(o) || o == ' ' || o == '.') => o,
            _ => c,
        })
        .collect()
}

fn is_reserved(c: char) -> bool {
    (c as u32) < 0x20 || matches!(c, '"' | '*' | ':' | '<' | '>' | '?' | '\\' | '|')
}

/// Windows FILETIME: 100 ns units since 1601.
fn filetime(ns: u64) -> u64 {
    ns / 100 + 116_444_736_000_000_000
}

impl Fs {
    fn info(&self, m: &Meta, name: &str) -> FileInfo {
        let bs = self.backend.block_size();
        let mut attrs = if m.is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_ARCHIVE };
        if m.hidden || name.starts_with('.') {
            attrs |= FILE_ATTRIBUTE_HIDDEN;
        }
        FileInfo {
            file_attributes: attrs,
            allocation_size: m.size.div_ceil(bs) * bs,
            file_size: m.size,
            creation_time: filetime(m.create),
            last_access_time: filetime(m.access),
            last_write_time: filetime(m.write),
            change_time: filetime(m.change),
            index_number: m.id,
            ..FileInfo::default()
        }
    }

    fn listing(&self, dir: u64) -> Result<Arc<Listing>, NTSTATUS> {
        if let Some(l) = self.dirs.lock().unwrap().get(&dir) {
            return Ok(Arc::clone(l));
        }
        let mut keyed: Vec<(String, Child)> = self
            .backend
            .list(dir)?
            .into_iter()
            .map(|(name, meta)| {
                let child =
                    Child { win_name: mac_to_win(&name).encode_utf16().collect(), info: self.info(&meta, &name), meta };
                (self.backend.name_key(&name), child)
            })
            .collect();
        keyed.sort_by(|a, b| a.1.win_name.cmp(&b.1.win_name));
        let by_key = keyed.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect();
        let listing = Arc::new(Listing { entries: keyed.into_iter().map(|(_, c)| c).collect(), by_key });
        let mut cache = self.dirs.lock().unwrap();
        if cache.len() >= DIR_CACHE_CAP {
            cache.clear();
        }
        cache.insert(dir, Arc::clone(&listing));
        Ok(listing)
    }

    /// Resolves a WinFsp path such as `\Folder\file.txt`.
    fn resolve(&self, path: &[u16]) -> Result<(Meta, FileInfo), NTSTATUS> {
        let path = String::from_utf16_lossy(path);
        let comps: Vec<&str> = path.split('\\').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            return Ok(self.root);
        }
        let cache_key = self.backend.name_key(&win_to_mac(&path));
        if let Some(hit) = self.paths.lock().unwrap().get(&cache_key) {
            return Ok(*hit);
        }
        let mut cur = self.root;
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            if !cur.0.is_dir {
                return Err(STATUS_OBJECT_PATH_NOT_FOUND);
            }
            let listing = self.listing(cur.0.id)?;
            let key = self.backend.name_key(&win_to_mac(comp));
            let Some(&idx) = listing.by_key.get(&key) else {
                return Err(if last { STATUS_OBJECT_NAME_NOT_FOUND } else { STATUS_OBJECT_PATH_NOT_FOUND });
            };
            let child = &listing.entries[idx];
            cur = (child.meta, child.info);
        }
        let mut cache = self.paths.lock().unwrap();
        if cache.len() >= PATH_CACHE_CAP {
            cache.clear();
        }
        cache.insert(cache_key, cur);
        Ok(cur)
    }

    unsafe fn copy_security(&self, sd: *mut c_void, sd_size: *mut usize) -> NTSTATUS {
        if sd_size.is_null() {
            return STATUS_SUCCESS;
        }
        let need = self.security.len();
        if *sd_size < need {
            *sd_size = need;
            return STATUS_BUFFER_OVERFLOW;
        }
        *sd_size = need;
        if !sd.is_null() {
            copy_nonoverlapping(self.security.as_ptr(), sd.cast::<u8>(), need);
        }
        STATUS_SUCCESS
    }

    /// Adds one entry to a ReadDirectory buffer; false when the buffer is full.
    unsafe fn add_entry(&self, name: &[u16], info: &FileInfo, buf: *mut c_void, len: u32, done: *mut u32) -> bool {
        #[repr(C)]
        struct Entry {
            hdr: DirInfo,
            name: [u16; NAME_MAX],
        }
        let n = name.len().min(NAME_MAX);
        let mut e = Entry {
            hdr: DirInfo { size: (size_of::<DirInfo>() + n * 2) as u16, file_info: *info, padding: [0; 24] },
            name: [0; NAME_MAX],
        };
        e.name[..n].copy_from_slice(&name[..n]);
        (self.api.add_dir_info)(&mut e.hdr, buf, len, done) != 0
    }
}

unsafe fn this<'a>(fs: *mut FileSystem) -> &'a Fs {
    &*((*fs).user_context as *const Fs)
}

unsafe fn wstr<'a>(p: *const u16) -> &'a [u16] {
    if p.is_null() {
        return &[];
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    std::slice::from_raw_parts(p, n)
}

/// Panics must not unwind into WinFsp; report them as an internal error.
fn guard(f: impl FnOnce() -> Result<(), NTSTATUS>) -> NTSTATUS {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => STATUS_SUCCESS,
        Ok(Err(status)) => status,
        Err(_) => STATUS_INTERNAL_ERROR,
    }
}

unsafe extern "system" fn get_volume_info(fs: *mut FileSystem, vi: *mut VolumeInfo) -> NTSTATUS {
    guard(|| {
        let me = this(fs);
        let vi = &mut *vi;
        (vi.total_size, vi.free_size) = me.backend.total_and_free();
        let n = me.label.len().min(vi.volume_label.len());
        vi.volume_label[..n].copy_from_slice(&me.label[..n]);
        vi.volume_label_length = (n * 2) as u16;
        Ok(())
    })
}

unsafe extern "system" fn get_security_by_name(
    fs: *mut FileSystem,
    name: *const u16,
    attrs: *mut u32,
    sd: *mut c_void,
    sd_size: *mut usize,
) -> NTSTATUS {
    guard(|| {
        let me = this(fs);
        let (_, info) = me.resolve(wstr(name))?;
        if !attrs.is_null() {
            *attrs = info.file_attributes;
        }
        match me.copy_security(sd, sd_size) {
            STATUS_SUCCESS => Ok(()),
            other => Err(other),
        }
    })
}

unsafe extern "system" fn open(
    fs: *mut FileSystem,
    name: *const u16,
    create_options: u32,
    _granted_access: u32,
    file_context: *mut *mut c_void,
    file_info: *mut FileInfo,
) -> NTSTATUS {
    guard(|| {
        let me = this(fs);
        let (meta, info) = me.resolve(wstr(name))?;
        if create_options & FILE_DIRECTORY_FILE != 0 && !meta.is_dir {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        if create_options & FILE_NON_DIRECTORY_FILE != 0 && meta.is_dir {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        *file_context = Box::into_raw(Box::new(Handle { meta, info, reader: OnceLock::new() })).cast();
        *file_info = info;
        Ok(())
    })
}

/// WinFsp will not dispatch opens unless Create and Overwrite exist, even on
/// a read-only volume; both just refuse.
unsafe extern "system" fn create(
    _fs: *mut FileSystem,
    _name: *const u16,
    _create_options: u32,
    _granted_access: u32,
    _attributes: u32,
    _sd: *mut c_void,
    _allocation_size: u64,
    _file_context: *mut *mut c_void,
    _file_info: *mut FileInfo,
) -> NTSTATUS {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn overwrite(
    _fs: *mut FileSystem,
    _file_context: *mut c_void,
    _attributes: u32,
    _replace_attributes: u8,
    _allocation_size: u64,
    _file_info: *mut FileInfo,
) -> NTSTATUS {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn close(_fs: *mut FileSystem, file_context: *mut c_void) {
    if !file_context.is_null() {
        drop(Box::from_raw(file_context.cast::<Handle>()));
    }
}

unsafe extern "system" fn read(
    fs: *mut FileSystem,
    file_context: *mut c_void,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    transferred: *mut u32,
) -> NTSTATUS {
    guard(|| {
        let me = this(fs);
        let h = &*(file_context as *const Handle);
        if h.meta.is_dir {
            return Err(STATUS_INVALID_DEVICE_REQUEST);
        }
        if offset >= h.meta.size {
            return Err(STATUS_END_OF_FILE);
        }
        // Readers are opened lazily: Explorer opens many files only to look at them.
        let reader = h
            .reader
            .get_or_init(|| me.backend.open(&h.meta).map(|r| ReadAhead::new(r, h.meta.size)))
            .as_ref()
            .map_err(|&s| s)?;
        let out = std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize);
        *transferred = reader.read_at(offset, out)? as u32;
        Ok(())
    })
}

unsafe extern "system" fn get_file_info(_fs: *mut FileSystem, file_context: *mut c_void, file_info: *mut FileInfo) -> NTSTATUS {
    *file_info = (*(file_context as *const Handle)).info;
    STATUS_SUCCESS
}

unsafe extern "system" fn get_security(
    fs: *mut FileSystem,
    _file_context: *mut c_void,
    sd: *mut c_void,
    sd_size: *mut usize,
) -> NTSTATUS {
    this(fs).copy_security(sd, sd_size)
}

unsafe extern "system" fn read_directory(
    fs: *mut FileSystem,
    file_context: *mut c_void,
    _pattern: *const u16,
    marker: *const u16,
    buffer: *mut c_void,
    length: u32,
    transferred: *mut u32,
) -> NTSTATUS {
    guard(|| {
        let me = this(fs);
        let h = &*(file_context as *const Handle);
        if !h.meta.is_dir {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        let listing = me.listing(h.meta.id)?;
        *transferred = 0;

        // Non-root directories start with "." and "..", then the sorted entries.
        // The marker is the last name returned by the previous call.
        const DOT: &[u16] = &[b'.' as u16];
        const DOTDOT: &[u16] = &[b'.' as u16, b'.' as u16];
        let is_root = h.meta.id == me.root.0.id;
        let (dots, start): (&[&[u16]], usize) = match (marker.is_null(), wstr(marker)) {
            (true, _) if is_root => (&[], 0),
            (true, _) => (&[DOT, DOTDOT], 0),
            (false, m) if m == DOT => (&[DOTDOT], 0),
            (false, m) if m == DOTDOT => (&[], 0),
            (false, m) => (&[], listing.entries.partition_point(|c| c.win_name.as_slice() <= m)),
        };
        for d in dots {
            if !me.add_entry(d, &h.info, buffer, length, transferred) {
                return Ok(());
            }
        }
        for c in &listing.entries[start..] {
            if !me.add_entry(&c.win_name, &c.info, buffer, length, transferred) {
                return Ok(());
            }
        }
        (me.api.add_dir_info)(null_mut(), buffer, length, transferred); // end of directory
        Ok(())
    })
}

#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl: *const u16,
        revision: u32,
        sd: *mut *mut c_void,
        len: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(p: *mut c_void) -> *mut c_void;
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    fn CreateEventW(attrs: *mut c_void, manual_reset: i32, initial: i32, name: *const u16) -> *mut c_void;
    fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

fn security_descriptor(sddl: &str) -> Result<Vec<u8>, String> {
    let mut p = null_mut();
    let mut len = 0u32;
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), 1, &mut p, &mut len) } == 0 {
        return Err(format!("could not build the security descriptor {sddl}"));
    }
    let sd = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len as usize) }.to_vec();
    unsafe { LocalFree(p) };
    Ok(sd)
}

/// A running mount; [`Mounted::stop`] takes it down.
pub struct Mounted {
    fs: *mut FileSystem,
    api: Api,
}

/// Mounts `backend` read-only at `mount_point` (`X:` or a folder that does
/// not exist yet) and starts serving requests on WinFsp's dispatcher threads.
pub fn mount(backend: &'static dyn Backend, mount_point: &str) -> Result<Mounted, String> {
    let api = Api::load()?;
    let root_meta = backend.root().map_err(|s| format!("cannot read the root folder ({s:#x})"))?;
    let fs: &'static mut Fs = Box::leak(Box::new(Fs {
        backend,
        api,
        security: security_descriptor(SDDL)?,
        label: backend.label().encode_utf16().collect(),
        root: (root_meta, FileInfo::default()),
        dirs: Mutex::new(HashMap::new()),
        paths: Mutex::new(HashMap::new()),
    }));
    fs.root.1 = fs.info(&root_meta, "");

    // SAFETY: VolumeParams is plain old data; all-zero is its documented default.
    let mut params: VolumeParams = unsafe { std::mem::zeroed() };
    params.version = size_of::<VolumeParams>() as u16;
    params.sector_size = 512;
    params.sectors_per_allocation_unit = (backend.block_size() / 512) as u16;
    params.max_component_length = 255;
    params.volume_creation_time = filetime(backend.created());
    params.volume_serial_number = backend.serial();
    // Infinite: nothing changes underneath a read-only mount, and WinFsp only
    // lets the Windows cache manager cache file data with an infinite timeout.
    params.file_info_timeout = u32::MAX;
    params.flags = VP_CASE_PRESERVED_NAMES
        | VP_UNICODE_ON_DISK
        | VP_PERSISTENT_ACLS
        | VP_READ_ONLY_VOLUME
        | VP_UM_FILE_CONTEXT_IS_USER_CONTEXT2;
    if !backend.case_insensitive() {
        params.flags |= VP_CASE_SENSITIVE_SEARCH;
    }
    for (dst, src) in params.file_system_name.iter_mut().zip("APFS".encode_utf16()) {
        *dst = src;
    }

    let iface: &'static mut Interface = Box::leak(Box::new(Interface { slots: [std::ptr::null(); 64] }));
    iface.slots[OP_GET_VOLUME_INFO] = get_volume_info as GetVolumeInfoFn as *const c_void;
    iface.slots[OP_GET_SECURITY_BY_NAME] = get_security_by_name as GetSecurityByNameFn as *const c_void;
    iface.slots[OP_CREATE] = create as CreateFileFn as *const c_void;
    iface.slots[OP_OPEN] = open as OpenFn as *const c_void;
    iface.slots[OP_OVERWRITE] = overwrite as OverwriteFn as *const c_void;
    iface.slots[OP_CLOSE] = close as CloseFn as *const c_void;
    iface.slots[OP_READ] = read as ReadFn as *const c_void;
    iface.slots[OP_GET_FILE_INFO] = get_file_info as GetFileInfoFn as *const c_void;
    iface.slots[OP_GET_SECURITY] = get_security as GetSecurityFn as *const c_void;
    iface.slots[OP_READ_DIRECTORY] = read_directory as ReadDirectoryFn as *const c_void;

    let mut handle: *mut FileSystem = null_mut();
    unsafe {
        let st = (api.create)(wide(DISK_DEVICE_NAME).as_ptr(), &params, iface, &mut handle);
        if st != STATUS_SUCCESS {
            return Err(format!("WinFsp could not create the file system (status {st:#010x})"));
        }
        (*handle).user_context = (fs as *const Fs).cast_mut().cast();
        if std::env::var_os("WINAPFS_DEBUG").is_some() {
            api.enable_debug_log(handle);
        }
        let st = (api.set_mount_point)(handle, wide(mount_point).as_ptr());
        if st != STATUS_SUCCESS {
            (api.delete)(handle);
            return Err(format!("WinFsp could not mount at {mount_point} (status {st:#010x})"));
        }
        let st = (api.start_dispatcher)(handle, 0);
        if st != STATUS_SUCCESS {
            (api.remove_mount_point)(handle);
            (api.delete)(handle);
            return Err(format!("WinFsp could not start serving requests (status {st:#010x})"));
        }
    }
    Ok(Mounted { fs: handle, api })
}

impl Mounted {
    pub fn stop(self) {
        unsafe {
            (self.api.stop_dispatcher)(self.fs);
            (self.api.remove_mount_point)(self.fs);
            (self.api.delete)(self.fs);
        }
    }
}

static STOP_EVENT: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static UNMOUNTED_EVENT: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

unsafe extern "system" fn on_console_ctrl(kind: u32) -> i32 {
    const CTRL_CLOSE_EVENT: u32 = 2;
    SetEvent(STOP_EVENT.load(Ordering::SeqCst));
    // On window close, logoff or shutdown Windows ends the process as soon as
    // this handler returns, so hold it until the unmount is done (Windows
    // allows about 5 seconds).
    if kind >= CTRL_CLOSE_EVENT {
        WaitForSingleObject(UNMOUNTED_EVENT.load(Ordering::SeqCst), 4500);
    }
    1
}

/// Call after the file system is unmounted, so a pending close can finish.
pub fn signal_unmounted() {
    let ev = UNMOUNTED_EVENT.load(Ordering::SeqCst);
    if !ev.is_null() {
        unsafe { SetEvent(ev) };
    }
}

/// Named event that `winapfs unmount` signals for this mount point.
fn stop_event_name(mount_point: &str) -> Vec<u16> {
    let p = std::path::absolute(mount_point).map_or_else(|_| mount_point.to_string(), |p| p.display().to_string());
    let norm = p.trim_end_matches('\\').to_lowercase();
    let hash = norm.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    wide(&format!("winapfs-stop-{hash:016x}"))
}

/// Blocks until Ctrl+C, the console window closing, or `winapfs unmount`.
pub fn wait_for_stop(mount_point: &str) -> Result<(), String> {
    unsafe {
        let ev = CreateEventW(null_mut(), 1, 0, stop_event_name(mount_point).as_ptr());
        if ev.is_null() {
            return Err("could not create the unmount event".into());
        }
        STOP_EVENT.store(ev, Ordering::SeqCst);
        UNMOUNTED_EVENT.store(CreateEventW(null_mut(), 1, 0, std::ptr::null()), Ordering::SeqCst);
        // A parent that ignores Ctrl+C passes that on to us; turn it back on first.
        SetConsoleCtrlHandler(None, 0);
        SetConsoleCtrlHandler(Some(on_console_ctrl), 1);
        WaitForSingleObject(ev, u32::MAX);
    }
    Ok(())
}

/// Asks the `winapfs mount` process serving `mount_point` to unmount.
pub fn request_stop(mount_point: &str) -> bool {
    const EVENT_MODIFY_STATE: u32 = 0x0002;
    unsafe {
        let ev = OpenEventW(EVENT_MODIFY_STATE, 0, stop_event_name(mount_point).as_ptr());
        if ev.is_null() {
            return false;
        }
        SetEvent(ev);
        CloseHandle(ev);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_names_survive_the_round_trip() {
        for name in ["plain.txt", "a:b", "say \"hi\"", "dust osv ", "Icon\r", "notes...", "Råfiler", "x?y*z|<>\\"] {
            let win = mac_to_win(name);
            assert!(!win.chars().any(is_reserved), "{name:?} -> {win:?}");
            assert!(!win.ends_with(' ') && !win.ends_with('.'), "{name:?} -> {win:?}");
            assert_eq!(win_to_mac(&win), name);
        }
        assert_eq!(mac_to_win("Råfiler"), "Råfiler");
        assert_eq!(mac_to_win("a.b"), "a.b");
    }

    /// Serves byte i = i % 251 and counts backend requests.
    struct Counting {
        calls: std::sync::atomic::AtomicUsize,
        size: u64,
    }

    impl FileReader for Counting {
        fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, NTSTATUS> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let n = (buf.len() as u64).min(self.size.saturating_sub(off)) as usize;
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = ((off + i as u64) % 251) as u8;
            }
            Ok(n)
        }
    }

    fn counting(size: u64) -> (ReadAhead, &'static Counting) {
        let inner: &'static Counting = Box::leak(Box::new(Counting { calls: 0.into(), size }));
        struct Shared(&'static Counting);
        impl FileReader for Shared {
            fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, NTSTATUS> {
                self.0.read_at(off, buf)
            }
        }
        (ReadAhead::new(Box::new(Shared(inner)), size), inner)
    }

    #[test]
    fn sequential_small_reads_become_few_large_fetches() {
        let size = 50 * 1024 * 1024 + 777;
        let (ra, inner) = counting(size);
        let mut buf = vec![0u8; 8192];
        let mut off = 0u64;
        while off < size {
            let n = ra.read_at(off, &mut buf).unwrap();
            assert!(buf[..n].iter().enumerate().all(|(i, &b)| b == ((off + i as u64) % 251) as u8), "bad data at {off}");
            off += n as u64;
        }
        let calls = inner.calls.load(Ordering::SeqCst);
        assert!(calls <= 12, "{calls} backend requests for {} 8 KiB reads", size / 8192);
    }

    #[test]
    fn random_reads_stay_correct() {
        let size = 20 * 1024 * 1024;
        let (ra, _) = counting(size);
        let mut seed = 12345u64;
        for _ in 0..500 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let off = seed % (size + 1000);
            let mut buf = vec![0u8; (seed >> 40) as usize % 300_000 + 1];
            let n = ra.read_at(off, &mut buf).unwrap();
            assert_eq!(n as u64, (buf.len() as u64).min(size.saturating_sub(off)));
            assert!(buf[..n].iter().enumerate().all(|(i, &b)| b == ((off + i as u64) % 251) as u8));
        }
    }

    #[test]
    fn filetime_epoch() {
        assert_eq!(filetime(0), 116_444_736_000_000_000);
    }
}
