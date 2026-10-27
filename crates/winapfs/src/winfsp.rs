//! Minimal binding to the WinFsp user-mode file system API, loaded at run
//! time from the WinFsp installation (so no import library or delay-load
//! linker support is needed).
//!
//! Layouts follow WinFsp 2.1 `inc/winfsp/fsctl.h` and `inc/winfsp/winfsp.h`;
//! the size assertions below are the same ones those headers make.
//!
//! WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
//! https://github.com/winfsp/winfsp

#![allow(clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::mem::size_of;

pub type NTSTATUS = i32;

pub const STATUS_SUCCESS: NTSTATUS = 0;
pub const STATUS_BUFFER_OVERFLOW: NTSTATUS = 0x8000_0005_u32 as i32;
pub const STATUS_END_OF_FILE: NTSTATUS = 0xC000_0011_u32 as i32;
pub const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = 0xC000_0034_u32 as i32;
pub const STATUS_OBJECT_PATH_NOT_FOUND: NTSTATUS = 0xC000_003A_u32 as i32;
pub const STATUS_INVALID_DEVICE_REQUEST: NTSTATUS = 0xC000_0010_u32 as i32;
pub const STATUS_MEDIA_WRITE_PROTECTED: NTSTATUS = 0xC000_00A2_u32 as i32;
pub const STATUS_FILE_IS_A_DIRECTORY: NTSTATUS = 0xC000_00BA_u32 as i32;
pub const STATUS_NOT_SUPPORTED: NTSTATUS = 0xC000_00BB_u32 as i32;
pub const STATUS_FILE_CORRUPT_ERROR: NTSTATUS = 0xC000_0102_u32 as i32;
pub const STATUS_NOT_A_DIRECTORY: NTSTATUS = 0xC000_0103_u32 as i32;
pub const STATUS_IO_DEVICE_ERROR: NTSTATUS = 0xC000_0185_u32 as i32;
pub const STATUS_INTERNAL_ERROR: NTSTATUS = 0xC000_00E5_u32 as i32;
pub const STATUS_OBJECT_NAME_INVALID: NTSTATUS = 0xC000_0033_u32 as i32;
pub const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = 0xC000_0035_u32 as i32;
pub const STATUS_DISK_FULL: NTSTATUS = 0xC000_007F_u32 as i32;
pub const STATUS_DIRECTORY_NOT_EMPTY: NTSTATUS = 0xC000_0101_u32 as i32;

pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;

pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x02;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
/// "Leave the attributes as they are" in SetBasicInfo.
pub const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

/// `FspCleanupDelete`: the last handle closed with delete pending.
pub const CLEANUP_DELETE: u32 = 0x01;

/// `FSP_FSCTL_DISK_DEVICE_NAME`
pub const DISK_DEVICE_NAME: &str = "WinFsp.Disk";

/// `FSP_FSCTL_VOLUME_PARAMS` (V1).
#[repr(C)]
pub struct VolumeParams {
    pub version: u16,
    pub sector_size: u16,
    pub sectors_per_allocation_unit: u16,
    pub max_component_length: u16,
    pub volume_creation_time: u64,
    pub volume_serial_number: u32,
    pub transact_timeout: u32,
    pub irp_timeout: u32,
    pub irp_capacity: u32,
    pub file_info_timeout: u32,
    /// Bitfield; see the `VP_*` constants.
    pub flags: u32,
    pub prefix: [u16; 192],
    pub file_system_name: [u16; 16],
    pub flags_v1: u32,
    pub volume_info_timeout: u32,
    pub dir_info_timeout: u32,
    pub security_timeout: u32,
    pub stream_info_timeout: u32,
    pub ea_timeout: u32,
    pub fsext_control_code: u32,
    pub reserved32: [u32; 1],
    pub reserved64: [u64; 2],
}

// Bit positions in `VolumeParams::flags`, in header declaration order (MSVC
// allocates bitfields from the least significant bit).
pub const VP_CASE_SENSITIVE_SEARCH: u32 = 1 << 0;
pub const VP_CASE_PRESERVED_NAMES: u32 = 1 << 1;
pub const VP_UNICODE_ON_DISK: u32 = 1 << 2;
pub const VP_PERSISTENT_ACLS: u32 = 1 << 3;
pub const VP_READ_ONLY_VOLUME: u32 = 1 << 9;
pub const VP_POST_CLEANUP_WHEN_MODIFIED_ONLY: u32 = 1 << 10;
pub const VP_FLUSH_AND_PURGE_ON_CLEANUP: u32 = 1 << 14;
pub const VP_UM_FILE_CONTEXT_IS_USER_CONTEXT2: u32 = 1 << 16;

/// `FSP_FSCTL_VOLUME_INFO`
#[repr(C)]
pub struct VolumeInfo {
    pub total_size: u64,
    pub free_size: u64,
    pub volume_label_length: u16,
    pub volume_label: [u16; 32],
}

/// `FSP_FSCTL_FILE_INFO`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FileInfo {
    pub file_attributes: u32,
    pub reparse_tag: u32,
    pub allocation_size: u64,
    pub file_size: u64,
    pub creation_time: u64,
    pub last_access_time: u64,
    pub last_write_time: u64,
    pub change_time: u64,
    pub index_number: u64,
    pub hard_links: u32,
    pub ea_size: u32,
}

/// `FSP_FSCTL_DIR_INFO` without its trailing name buffer.
#[repr(C)]
pub struct DirInfo {
    pub size: u16,
    pub file_info: FileInfo,
    pub padding: [u8; 24],
}

/// Leading fields of `FSP_FILE_SYSTEM`; the rest is private to WinFsp.
#[repr(C)]
pub struct FileSystem {
    pub version: u16,
    pub user_context: *mut c_void,
}

const _: () = assert!(size_of::<VolumeParams>() == 504);
const _: () = assert!(size_of::<VolumeInfo>() == 88);
const _: () = assert!(size_of::<FileInfo>() == 72);
const _: () = assert!(size_of::<DirInfo>() == 104);

/// `FSP_FILE_SYSTEM_INTERFACE`: 64 operation slots. Unused slots stay null,
/// which WinFsp answers with STATUS_INVALID_DEVICE_REQUEST.
#[repr(C)]
pub struct Interface {
    pub slots: [*const c_void; 64],
}

const _: () = assert!(size_of::<Interface>() == 64 * size_of::<usize>());

// Slot indices, in header declaration order.
pub const OP_GET_VOLUME_INFO: usize = 0;
pub const OP_GET_SECURITY_BY_NAME: usize = 2;
pub const OP_CREATE: usize = 3;
pub const OP_OPEN: usize = 4;
pub const OP_OVERWRITE: usize = 5;
pub const OP_CLEANUP: usize = 6;
pub const OP_CLOSE: usize = 7;
pub const OP_READ: usize = 8;
pub const OP_WRITE: usize = 9;
pub const OP_FLUSH: usize = 10;
pub const OP_GET_FILE_INFO: usize = 11;
pub const OP_SET_BASIC_INFO: usize = 12;
pub const OP_SET_FILE_SIZE: usize = 13;
pub const OP_CAN_DELETE: usize = 14;
pub const OP_RENAME: usize = 15;
pub const OP_GET_SECURITY: usize = 16;
pub const OP_READ_DIRECTORY: usize = 18;

pub type GetVolumeInfoFn = unsafe extern "system" fn(*mut FileSystem, *mut VolumeInfo) -> NTSTATUS;
pub type GetSecurityByNameFn =
    unsafe extern "system" fn(*mut FileSystem, *const u16, *mut u32, *mut c_void, *mut usize) -> NTSTATUS;
pub type CreateFileFn = unsafe extern "system" fn(
    *mut FileSystem,
    *const u16,
    u32,
    u32,
    u32,
    *mut c_void,
    u64,
    *mut *mut c_void,
    *mut FileInfo,
) -> NTSTATUS;
pub type OpenFn = unsafe extern "system" fn(*mut FileSystem, *const u16, u32, u32, *mut *mut c_void, *mut FileInfo) -> NTSTATUS;
pub type OverwriteFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, u32, u8, u64, *mut FileInfo) -> NTSTATUS;
pub type CloseFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void);
pub type ReadFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, *mut c_void, u64, u32, *mut u32) -> NTSTATUS;
pub type GetFileInfoFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, *mut FileInfo) -> NTSTATUS;
pub type GetSecurityFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, *mut c_void, *mut usize) -> NTSTATUS;
pub type ReadDirectoryFn =
    unsafe extern "system" fn(*mut FileSystem, *mut c_void, *const u16, *const u16, *mut c_void, u32, *mut u32) -> NTSTATUS;
pub type CleanupFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, *const u16, u32);
/// (file system, file, buffer, offset, length, write to end of file, constrained I/O, transferred, info)
pub type WriteFn =
    unsafe extern "system" fn(*mut FileSystem, *mut c_void, *mut c_void, u64, u32, u8, u8, *mut u32, *mut FileInfo) -> NTSTATUS;
pub type FlushFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, *mut FileInfo) -> NTSTATUS;
/// (file system, file, attributes, creation, last access, last write, change, info)
pub type SetBasicInfoFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, u32, u64, u64, u64, u64, *mut FileInfo) -> NTSTATUS;
pub type SetFileSizeFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, u64, u8, *mut FileInfo) -> NTSTATUS;
pub type CanDeleteFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, *const u16) -> NTSTATUS;
pub type RenameFn = unsafe extern "system" fn(*mut FileSystem, *mut c_void, *const u16, *const u16, u8) -> NTSTATUS;

type CreateFn = unsafe extern "system" fn(*const u16, *const VolumeParams, *const Interface, *mut *mut FileSystem) -> NTSTATUS;
type DeleteFn = unsafe extern "system" fn(*mut FileSystem);
type SetMountPointFn = unsafe extern "system" fn(*mut FileSystem, *const u16) -> NTSTATUS;
type RemoveMountPointFn = unsafe extern "system" fn(*mut FileSystem);
type StartDispatcherFn = unsafe extern "system" fn(*mut FileSystem, u32) -> NTSTATUS;
type StopDispatcherFn = unsafe extern "system" fn(*mut FileSystem);
type AddDirInfoFn = unsafe extern "system" fn(*mut DirInfo, *mut c_void, u32, *mut u32) -> u8;
type SetDebugLogFn = unsafe extern "system" fn(*mut FileSystem, u32);
type DebugLogSetHandleFn = unsafe extern "system" fn(*mut c_void);

/// The WinFsp DLL entry points we use.
#[derive(Clone, Copy)]
pub struct Api {
    pub create: CreateFn,
    pub delete: DeleteFn,
    pub set_mount_point: SetMountPointFn,
    pub remove_mount_point: RemoveMountPointFn,
    pub start_dispatcher: StartDispatcherFn,
    pub stop_dispatcher: StopDispatcherFn,
    pub add_dir_info: AddDirInfoFn,
    set_debug_log: SetDebugLogFn,
    debug_log_set_handle: DebugLogSetHandleFn,
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

#[link(name = "advapi32")]
extern "system" {
    fn RegGetValueW(
        key: isize,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> i32;
}

const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const RRF_SUBKEY_WOW6432KEY: u32 = 0x0002_0000;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// WinFsp's install folder, from the registry key its installer writes.
fn install_dir() -> Option<String> {
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            wide("SOFTWARE\\WinFsp").as_ptr(),
            wide("InstallDir").as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6432KEY,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// The loaded WinFsp DLL.
struct Module {
    handle: *mut c_void,
    path: String,
}

impl Module {
    fn load() -> Result<Self, String> {
        let dir = install_dir().ok_or("WinFsp is not installed (no InstallDir in the registry)")?;
        let path = format!("{}\\bin\\winfsp-x64.dll", dir.trim_end_matches('\\'));
        let handle = unsafe { LoadLibraryW(wide(&path).as_ptr()) };
        if handle.is_null() {
            return Err(format!("could not load {path}"));
        }
        Ok(Self { handle, path })
    }

    /// SAFETY: `F` must be the function pointer type the header declares for `name`.
    unsafe fn sym<F: Copy>(&self, name: &str) -> Result<F, String> {
        assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
        let c = format!("{name}\0");
        let p = GetProcAddress(self.handle, c.as_ptr());
        if p.is_null() {
            return Err(format!("{} has no {name}", self.path));
        }
        Ok(std::mem::transmute_copy::<*mut c_void, F>(&p))
    }
}

impl Api {
    pub fn load() -> Result<Self, String> {
        let m = Module::load()?;
        // SAFETY: each symbol gets the signature its header declares.
        unsafe {
            Ok(Self {
                create: m.sym("FspFileSystemCreate")?,
                delete: m.sym("FspFileSystemDelete")?,
                set_mount_point: m.sym("FspFileSystemSetMountPoint")?,
                remove_mount_point: m.sym("FspFileSystemRemoveMountPoint")?,
                start_dispatcher: m.sym("FspFileSystemStartDispatcher")?,
                stop_dispatcher: m.sym("FspFileSystemStopDispatcher")?,
                add_dir_info: m.sym("FspFileSystemAddDirInfo")?,
                set_debug_log: m.sym("FspFileSystemSetDebugLogF")?,
                debug_log_set_handle: m.sym("FspDebugLogSetHandle")?,
            })
        }
    }

    /// Sends WinFsp's per-request log (every operation and its status) to stderr.
    pub unsafe fn enable_debug_log(&self, fs: *mut FileSystem) {
        const STD_ERROR_HANDLE: u32 = -12_i32 as u32;
        (self.debug_log_set_handle)(GetStdHandle(STD_ERROR_HANDLE));
        (self.set_debug_log)(fs, u32::MAX);
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn GetStdHandle(which: u32) -> *mut c_void;
}

/// `FSP_LAUNCH_REG_RECORD` without its trailing buffer: one launcher service
/// entry under `HKLM\Software\WinFsp\Services\<class>` (32-bit registry view).
#[repr(C)]
pub struct LaunchRecord {
    pub agent: *const u16,
    pub executable: *const u16,
    pub command_line: *const u16,
    pub work_directory: *const u16,
    pub run_as: *const u16,
    pub security: *const u16,
    pub auth_package: *const u16,
    pub stderr: *const u16,
    pub reserved0: [*const c_void; 4],
    pub job_control: u32,
    pub credentials: u32,
    pub auth_package_id: u32,
    pub recovery: u32,
    pub reserved1: [u32; 4],
}

const _: () = assert!(size_of::<LaunchRecord>() == 12 * size_of::<usize>() + 32);

type LaunchStartFn = unsafe extern "system" fn(*const u16, *const u16, u32, *const *const u16, u8, *mut u32) -> NTSTATUS;
type LaunchStopFn = unsafe extern "system" fn(*const u16, *const u16, *mut u32) -> NTSTATUS;
type LaunchGetNameListFn = unsafe extern "system" fn(*mut u16, *mut u32, *mut u32) -> NTSTATUS;
type LaunchGetInfoFn = unsafe extern "system" fn(*const u16, *const u16, *mut u16, *mut u32, *mut u32) -> NTSTATUS;
type LaunchRegSetRecordFn = unsafe extern "system" fn(*const u16, *const LaunchRecord) -> NTSTATUS;
type LaunchRegGetRecordFn = unsafe extern "system" fn(*const u16, *const u16, *mut *mut LaunchRecord) -> NTSTATUS;
type LaunchRegFreeRecordFn = unsafe extern "system" fn(*mut LaunchRecord);

/// What a launcher call failed with.
#[derive(Debug)]
pub enum LaunchError {
    /// Could not talk to the launcher service (NTSTATUS).
    Pipe(NTSTATUS),
    /// The launcher refused (Win32 error code).
    Launcher(u32),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::Pipe(st) => write!(f, "could not reach the WinFsp launcher service (status {st:#010x})"),
            LaunchError::Launcher(2) => write!(f, "winapfs is not set up with the WinFsp launcher"),
            LaunchError::Launcher(5) => write!(f, "this account may not start or stop winapfs drives"),
            LaunchError::Launcher(183) => write!(f, "that drive is already mounted"),
            LaunchError::Launcher(e) => write!(f, "the WinFsp launcher refused (Windows error {e})"),
        }
    }
}

/// WinFsp's launcher: a service, running as SYSTEM, that starts and stops
/// registered file systems on request.
pub struct Launcher {
    start: LaunchStartFn,
    stop: LaunchStopFn,
    name_list: LaunchGetNameListFn,
    get_info: LaunchGetInfoFn,
    reg_set: LaunchRegSetRecordFn,
    reg_get: LaunchRegGetRecordFn,
    reg_free: LaunchRegFreeRecordFn,
}

fn launch_result(st: NTSTATUS, err: u32) -> Result<(), LaunchError> {
    if st != STATUS_SUCCESS {
        Err(LaunchError::Pipe(st))
    } else if err != 0 {
        Err(LaunchError::Launcher(err))
    } else {
        Ok(())
    }
}

impl Launcher {
    pub fn load() -> Result<Self, String> {
        let m = Module::load()?;
        // SAFETY: each symbol gets the signature its header declares.
        unsafe {
            Ok(Self {
                start: m.sym("FspLaunchStart")?,
                stop: m.sym("FspLaunchStop")?,
                name_list: m.sym("FspLaunchGetNameList")?,
                get_info: m.sym("FspLaunchGetInfo")?,
                reg_set: m.sym("FspLaunchRegSetRecord")?,
                reg_get: m.sym("FspLaunchRegGetRecord")?,
                reg_free: m.sym("FspLaunchRegFreeRecord")?,
            })
        }
    }

    pub fn start(&self, class: &str, instance: &str, args: &[String]) -> Result<(), LaunchError> {
        let wide_args: Vec<Vec<u16>> = args.iter().map(|a| wide(a)).collect();
        let argv: Vec<*const u16> = wide_args.iter().map(|a| a.as_ptr()).collect();
        let mut err = 0u32;
        let st = unsafe {
            (self.start)(wide(class).as_ptr(), wide(instance).as_ptr(), argv.len() as u32, argv.as_ptr(), 0, &mut err)
        };
        launch_result(st, err)
    }

    pub fn stop(&self, class: &str, instance: &str) -> Result<(), LaunchError> {
        let mut err = 0u32;
        let st = unsafe { (self.stop)(wide(class).as_ptr(), wide(instance).as_ptr(), &mut err) };
        launch_result(st, err)
    }

    /// Running service instances as (class, instance) pairs.
    pub fn running(&self) -> Result<Vec<(String, String)>, LaunchError> {
        let mut buf = vec![0u16; 32 * 1024];
        let mut size = (buf.len() * 2) as u32;
        let mut err = 0u32;
        let st = unsafe { (self.name_list)(buf.as_mut_ptr(), &mut size, &mut err) };
        launch_result(st, err)?;
        let units = &buf[..(size as usize / 2).min(buf.len())];
        let strings: Vec<String> =
            units.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect();
        Ok(strings.chunks_exact(2).map(|p| (p[0].clone(), p[1].clone())).collect())
    }

    /// What the launcher reports about a running instance, as its list of strings.
    pub fn info(&self, class: &str, instance: &str) -> Result<Vec<String>, LaunchError> {
        let mut buf = vec![0u16; 8 * 1024];
        let mut size = (buf.len() * 2) as u32;
        let mut err = 0u32;
        let st = unsafe { (self.get_info)(wide(class).as_ptr(), wide(instance).as_ptr(), buf.as_mut_ptr(), &mut size, &mut err) };
        launch_result(st, err)?;
        let units = &buf[..(size as usize / 2).min(buf.len())];
        Ok(units.split(|&c| c == 0).map(String::from_utf16_lossy).collect())
    }

    pub fn is_registered(&self, class: &str) -> bool {
        let mut rec: *mut LaunchRecord = std::ptr::null_mut();
        let st = unsafe { (self.reg_get)(wide(class).as_ptr(), std::ptr::null(), &mut rec) };
        if st == STATUS_SUCCESS && !rec.is_null() {
            unsafe { (self.reg_free)(rec) };
            return true;
        }
        false
    }

    /// Writes (or with `None`, deletes) the service entry. Needs administrator rights.
    pub fn set_record(&self, class: &str, record: Option<&LaunchRecord>) -> Result<(), NTSTATUS> {
        let st = unsafe { (self.reg_set)(wide(class).as_ptr(), record.map_or(std::ptr::null(), |r| r as *const _)) };
        if st == STATUS_SUCCESS { Ok(()) } else { Err(st) }
    }
}
