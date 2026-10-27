//! Mounts Mac drives as they are plugged in. Runs as a Windows service
//! (`winapfs-automount`, started at boot) that polls for disks with an APFS
//! partition and asks the WinFsp launcher to mount or unmount them.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::Duration;

use crate::disks;
use crate::service::{install_dir, CLASS};
use crate::winfsp::{wide, Launcher};
use crate::{CliError, CliResult};

pub const SERVICE_NAME: &str = "winapfs-automount";
const POLL: Duration = Duration::from_secs(3);
/// Drive letters to use, in order of preference.
const LETTERS: &[u8] = b"MNOPQRSTUVWXYZ";
const LOG_CAP: u64 = 1 << 20;

static STOP: AtomicBool = AtomicBool::new(false);

/// What the watcher remembers between polls.
#[derive(Default)]
struct State {
    /// Disks this watcher mounted, by disk number (unmounted when it stops).
    ours: HashMap<u32, String>,
    /// Every disk that was mounted at the last poll, by whoever mounted it.
    seen: HashMap<u32, String>,
    /// Disks not to mount again until they are unplugged: someone unmounted
    /// them, or mounting failed.
    declined: HashSet<u32>,
}

/// One poll: compare connected Mac disks with running mounts and act.
fn poll(l: &Launcher, st: &mut State, log: &mut dyn FnMut(&str)) {
    let present: Vec<u32> = disks::apfs_disks();
    let running: Vec<String> =
        l.running().unwrap_or_default().into_iter().filter(|(c, _)| c == CLASS).map(|(_, i)| i).collect();
    // Which disk each running mount serves, from its command line (`src=diskN`;
    // `src=auto` means the first Mac disk, as `serve` resolves it).
    let served: HashMap<u32, String> = running
        .iter()
        .filter_map(|inst| {
            let info = l.info(CLASS, inst).ok()?;
            let src = info.iter().find_map(|s| {
                s.split(' ').find_map(|tok| tok.trim_matches('"').strip_prefix("src=").map(str::to_string))
            })?;
            let disk = match src.strip_prefix("disk") {
                Some(n) => n.parse().ok()?,
                None if src == "auto" => *present.first()?,
                None => return None,
            };
            Some((disk, inst.clone()))
        })
        .collect();

    st.declined.retain(|d| present.contains(d));
    // A mount that went away while its disk is still plugged in was unmounted
    // on purpose (or failed to start): don't fight it.
    for (d, letter) in std::mem::take(&mut st.seen) {
        if !served.contains_key(&d) && present.contains(&d) {
            log(&format!("{letter}: (disk{d}) was unmounted; leaving it until it is plugged in again"));
            st.declined.insert(d);
            st.ours.remove(&d);
        }
    }
    for (d, letter) in &served {
        if !present.contains(d) {
            log(&format!("disk{d} was unplugged; unmounting {letter}:"));
            let _ = l.stop(CLASS, letter);
            st.ours.remove(d);
        }
    }
    st.seen = served.iter().filter(|(d, _)| present.contains(d)).map(|(&d, l)| (d, l.clone())).collect();
    for &d in &present {
        if served.contains_key(&d) || st.declined.contains(&d) {
            continue;
        }
        let Some(letter) = free_letter() else {
            log("no free drive letter for a Mac drive");
            break;
        };
        let mut args = vec![format!("src=disk{d}"), format!("at={letter}:"), "vol=0".to_string()];
        if crate::service::write_mode() {
            args.push("mode=write".to_string());
        }
        match l.start(CLASS, &letter, &args) {
            Ok(()) => {
                log(&format!("disk{d} plugged in; mounting at {letter}:"));
                st.seen.insert(d, letter.clone());
                st.ours.insert(d, letter);
            }
            Err(e) => {
                log(&format!("could not mount disk{d}: {e}"));
                st.declined.insert(d);
            }
        }
    }
}

fn free_letter() -> Option<String> {
    LETTERS.iter().map(|&c| (c as char).to_string()).find(|l| !Path::new(&format!("{l}:\\")).exists())
}

/// The watch loop; returns when `STOP` is set, after unmounting what it mounted.
fn watch(log: &mut dyn FnMut(&str)) -> Result<(), String> {
    let l = Launcher::load()?;
    let mut st = State::default();
    log("watching for Mac drives");
    while !STOP.load(Ordering::SeqCst) {
        poll(&l, &mut st, log);
        let mut waited = Duration::ZERO;
        while waited < POLL && !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
            waited += Duration::from_millis(200);
        }
    }
    for letter in st.ours.values() {
        let _ = l.stop(CLASS, letter);
    }
    log("stopped");
    Ok(())
}

/// `winapfs automount-run`: the watch loop in this terminal, for testing.
pub fn run_in_terminal() -> CliResult {
    unsafe {
        SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
    watch(&mut |m| println!("{m}")).map_err(CliError::new)
}

unsafe extern "system" fn on_ctrl(_kind: u32) -> i32 {
    STOP.store(true, Ordering::SeqCst);
    1
}

/// Appends to `automount.log` next to the installed program, keeping it small.
fn file_logger() -> impl FnMut(&str) {
    let path = install_dir().join("automount.log");
    move |msg: &str| {
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_CAP) {
            let _ = std::fs::remove_file(&path);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{}  {msg}", timestamp());
        }
    }
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    crate::format_time(secs * 1_000_000_000) + " UTC"
}

// --- Windows service plumbing ------------------------------------------------

#[repr(C)]
struct ServiceTableEntry {
    name: *mut u16,
    main: Option<unsafe extern "system" fn(u32, *mut *mut u16)>,
}

#[repr(C)]
struct ServiceStatus {
    service_type: u32,
    current_state: u32,
    controls_accepted: u32,
    win32_exit_code: u32,
    service_exit_code: u32,
    check_point: u32,
    wait_hint: u32,
}

type ControlHandler = unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32;

#[link(name = "advapi32")]
extern "system" {
    fn StartServiceCtrlDispatcherW(table: *const ServiceTableEntry) -> i32;
    fn RegisterServiceCtrlHandlerExW(name: *const u16, handler: Option<ControlHandler>, context: *mut c_void) -> *mut c_void;
    fn SetServiceStatus(handle: *mut c_void, status: *const ServiceStatus) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

const SERVICE_WIN32_OWN_PROCESS: u32 = 0x10;
const SERVICE_STOPPED: u32 = 1;
const SERVICE_STOP_PENDING: u32 = 3;
const SERVICE_RUNNING: u32 = 4;
const SERVICE_ACCEPT_STOP: u32 = 0x1;
const SERVICE_ACCEPT_SHUTDOWN: u32 = 0x4;
const SERVICE_CONTROL_STOP: u32 = 1;
const SERVICE_CONTROL_INTERROGATE: u32 = 4;
const SERVICE_CONTROL_SHUTDOWN: u32 = 5;
const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;

static STATUS_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn set_status(state: u32) {
    let status = ServiceStatus {
        service_type: SERVICE_WIN32_OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        win32_exit_code: 0,
        service_exit_code: 0,
        check_point: 0,
        wait_hint: if state == SERVICE_STOP_PENDING { 10_000 } else { 0 },
    };
    unsafe { SetServiceStatus(STATUS_HANDLE.load(Ordering::SeqCst), &status) };
}

unsafe extern "system" fn control(code: u32, _event: u32, _data: *mut c_void, _ctx: *mut c_void) -> u32 {
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_status(SERVICE_STOP_PENDING);
            STOP.store(true, Ordering::SeqCst);
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let name = wide(SERVICE_NAME);
    STATUS_HANDLE.store(RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), std::ptr::null_mut()), Ordering::SeqCst);
    set_status(SERVICE_RUNNING);
    let mut log = file_logger();
    if let Err(e) = watch(&mut log) {
        log(&format!("error: {e}"));
    }
    set_status(SERVICE_STOPPED);
}

/// `winapfs automount-service`: entry point when the service manager starts us.
pub fn run_as_service() -> CliResult {
    let mut name = wide(SERVICE_NAME);
    let table = [
        ServiceTableEntry { name: name.as_mut_ptr(), main: Some(service_main) },
        ServiceTableEntry { name: std::ptr::null_mut(), main: None },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(CliError::new("automount-service is started by Windows; to test, run `winapfs automount-run`"));
    }
    Ok(())
}
