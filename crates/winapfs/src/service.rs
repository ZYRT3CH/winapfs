//! Drive letters that Explorer can see, through WinFsp's launcher.
//!
//! Reading a raw disk needs administrator rights, but a drive letter created
//! by an elevated process is invisible to Explorer, which runs unelevated.
//! WinFsp's launcher is a service running as SYSTEM: `winapfs setup`
//! registers winapfs with it once (as administrator), after which `winapfs
//! mount disk3 Z:` from a normal terminal asks the launcher to run
//! `winapfs serve` as SYSTEM, and SYSTEM's drive letters are visible to everyone.

use std::ffi::c_void;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::automount::SERVICE_NAME;
use crate::disks;
use crate::winfsp::{wide, LaunchRecord, Launcher};
use crate::{cmd_mount, source_path, CliError, CliResult, MountMode, WINFSP_NOTICE};

/// Launcher service class name, and the key under `HKLM\Software\WinFsp\Services`.
pub(crate) const CLASS: &str = "winapfs";

pub fn is_drive_letter(mount_point: &str) -> bool {
    let b = mount_point.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// `disk3`, `disk:3`, `PhysicalDrive3` or `\\.\PhysicalDrive3`, as `disk3`;
/// `auto` (find the disk with an APFS partition) stays `auto`.
pub fn disk_source(src: &str) -> Option<String> {
    if src.eq_ignore_ascii_case("auto") {
        return Some("auto".into());
    }
    source_path(src).strip_prefix(r"\\.\PhysicalDrive").map(|n| format!("disk{n}"))
}

fn instance(mount_point: &str) -> String {
    mount_point[..1].to_ascii_uppercase()
}

/// Where setup installs the copy of winapfs that the launcher runs. Only
/// administrators can change files there; the launcher runs it as SYSTEM, so
/// a user-writable location would hand SYSTEM rights to anyone who swaps the file.
pub(crate) fn install_dir() -> PathBuf {
    let base = std::env::var_os("ProgramFiles").map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from);
    base.join("winapfs")
}

fn launcher() -> Result<Launcher, CliError> {
    Launcher::load().map_err(|e| CliError::new(e).hint("install WinFsp from https://winfsp.dev"))
}

pub fn registered() -> bool {
    Launcher::load().is_ok_and(|l| l.is_registered(CLASS))
}

fn running_here(l: &Launcher, inst: &str) -> bool {
    l.running().is_ok_and(|r| r.iter().any(|(c, i)| c == CLASS && i.eq_ignore_ascii_case(inst)))
}

fn admin_needed(what: &str) -> CliError {
    CliError::new(format!("{what} needs administrator rights"))
        .hint("run it from an Administrator terminal (right-click Terminal > Run as administrator)")
}

/// Stops every running winapfs drive and waits for the drive letters to go.
fn unmount_everything(l: &Launcher) {
    let drives: Vec<String> =
        l.running().unwrap_or_default().into_iter().filter(|(c, _)| c == CLASS).map(|(_, i)| i).collect();
    for d in &drives {
        let _ = l.stop(CLASS, d);
    }
    let t = Instant::now();
    while drives.iter().any(|d| Path::new(&format!("{d}:\\")).exists()) && t.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn sc(args: &[&str]) -> io::Result<std::process::Output> {
    Command::new("sc.exe").args(args).output()
}

fn automount_installed() -> bool {
    sc(&["query", SERVICE_NAME]).is_ok_and(|o| o.status.success())
}

pub(crate) fn automount_running() -> bool {
    sc(&["query", SERVICE_NAME]).is_ok_and(|o| {
        let text = String::from_utf8_lossy(&o.stdout);
        o.status.success() && text.lines().any(|l| l.contains("STATE") && l.contains(" 4 "))
    })
}

/// Stops the auto-mount service (if installed) and waits until it has exited.
fn stop_automount() {
    if !automount_installed() {
        return;
    }
    let _ = sc(&["stop", SERVICE_NAME]);
    let t = Instant::now();
    while automount_running() && t.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// `winapfs setup [--remove]`
pub fn setup(remove: bool) -> CliResult {
    // Check first: a half-run setup without rights could still unmount drives.
    if !is_elevated() {
        return Err(admin_needed(if remove { "removal" } else { "setup" })
            .hint("or run `winapfs install`, which asks for permission itself"));
    }
    let l = launcher()?;
    let dir = install_dir();
    let exe = dir.join("winapfs.exe");
    let log = dir.join("serve.log");
    if remove {
        stop_automount();
        if automount_installed() && !sc(&["delete", SERVICE_NAME]).is_ok_and(|o| o.status.success()) {
            return Err(admin_needed("removing the auto-mount service"));
        }
        unmount_everything(&l);
        l.set_record(CLASS, None).map_err(|_| admin_needed("removing the launcher entry"))?;
        if let Err(e) = set_system_path(&dir, false) {
            println!("note      {e}; remove {} from PATH by hand", dir.display());
        }
        for f in [&exe, &log, &dir.join("automount.log")] {
            let _ = fs::remove_file(f);
        }
        let _ = fs::remove_dir(&dir);
        println!("removed   the auto-mount service, the launcher entry, the PATH entry and {}", dir.display());
        return Ok(());
    }

    fs::create_dir_all(&dir).map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => admin_needed("setup"),
        _ => e.into(),
    })?;
    let me = std::env::current_exe()?;
    let already_there = fs::canonicalize(&me).ok() == fs::canonicalize(&exe).ok();
    if !already_there {
        // An update replaces a program that the auto-mount service and any
        // mounted drive are running, so stop those first.
        stop_automount();
        unmount_everything(&l);
        let mut attempt = 0;
        while let Err(e) = fs::copy(&me, &exe) {
            attempt += 1;
            if e.kind() == io::ErrorKind::PermissionDenied && !exe.exists() || attempt >= 20 {
                return Err(match e.kind() {
                    io::ErrorKind::PermissionDenied => admin_needed("setup"),
                    _ => CliError::new(format!("could not install {}: {e}", exe.display())),
                });
            }
            std::thread::sleep(Duration::from_millis(500)); // the old program may still be exiting
        }
    }
    if !log.exists() {
        fs::write(&log, b"")?;
    }

    let sid = current_user_sid().map_err(CliError::new)?;
    // Start, stop and query rights: SYSTEM, Administrators, and the account running setup.
    let security = format!("D:P(A;;RPWPLC;;;SY)(A;;RPWPLC;;;BA)(A;;RPWPLC;;;{sid})");
    let (exe_w, cmd_w, dir_w, sec_w, log_w) = (
        wide(&exe.display().to_string()),
        wide("serve %1 %2 %3 %4 %5 %6"),
        wide(&dir.display().to_string()),
        wide(&security),
        wide(&log.display().to_string()),
    );
    let record = LaunchRecord {
        agent: std::ptr::null(),
        executable: exe_w.as_ptr(),
        command_line: cmd_w.as_ptr(),
        work_directory: dir_w.as_ptr(),
        run_as: std::ptr::null(), // default: LocalSystem, which can read raw disks
        security: sec_w.as_ptr(),
        auth_package: std::ptr::null(),
        stderr: log_w.as_ptr(),
        reserved0: [std::ptr::null(); 4],
        job_control: 1, // the launcher ends the whole process tree when an instance stops
        credentials: 0,
        auth_package_id: 0,
        recovery: 0,
        reserved1: [0; 4],
    };
    l.set_record(CLASS, Some(&record)).map_err(|_| admin_needed("registering with the WinFsp launcher"))?;

    // The auto-mount service: starts with Windows, after the WinFsp launcher.
    let bin = format!("\"{}\" automount-service", exe.display());
    let verb = if automount_installed() { "config" } else { "create" };
    let made = sc(&[verb, SERVICE_NAME, "binPath=", &bin, "start=", "auto", "depend=", "WinFsp.Launcher", "DisplayName=", "winapfs auto-mount"])?;
    if !made.status.success() {
        return Err(CliError::new(format!("could not set up the auto-mount service: {}", String::from_utf8_lossy(&made.stdout).trim())));
    }
    let _ = sc(&["description", SERVICE_NAME, "Shows Mac (APFS) drives in Explorer, read-only, when they are plugged in."]);
    let _ = sc(&["start", SERVICE_NAME]);
    if let Err(e) = set_system_path(&dir, true) {
        println!("note      {e}; the winapfs command works from {}", dir.display());
    }

    println!("installed {}  (on the system PATH; open a new terminal to use `winapfs`)", exe.display());
    println!("registered with the WinFsp launcher; it runs winapfs as SYSTEM, read-only");
    println!("auto-mount service: {}", if automount_running() { "running, starts with Windows" } else { "installed but NOT running" });
    println!("allowed   to mount and unmount: your account ({sid}), Administrators, SYSTEM");
    println!("limits    the service mounts only whole disks (diskN) at drive letters");
    println!("logs      {}", dir.display());
    println!("undo:     winapfs uninstall");
    Ok(())
}

/// `winapfs install`: everything a new user needs, asking for admin rights once.
pub fn install() -> CliResult {
    println!("winapfs shows Mac (APFS) drives in Windows Explorer. It only reads; it never changes the Mac drive.");
    println!();
    if Launcher::load().is_err() {
        println!("winapfs needs WinFsp, a free helper that lets programs act as drives (https://winfsp.dev).");
        if !ask("Install WinFsp now?") {
            return Err(CliError::new("WinFsp is needed").hint("install it from https://winfsp.dev/rel/, then run winapfs again"));
        }
        let installed = Command::new("winget")
            .args(["install", "--id", "WinFsp.WinFsp", "-e", "--accept-package-agreements", "--accept-source-agreements"])
            .status()
            .is_ok_and(|s| s.success());
        if !installed || Launcher::load().is_err() {
            let _ = Command::new("cmd").args(["/c", "start", "", "https://winfsp.dev/rel/"]).status();
            return Err(CliError::new("could not install WinFsp automatically")
                .hint("install it from the web page that just opened, then run winapfs again"));
        }
        println!();
    }

    println!("Setting up. Windows will ask for permission once.");
    if is_elevated() {
        setup(false)?;
    } else {
        run_elevated("setup")?;
    }
    if !registered() || !automount_running() {
        return Err(CliError::new("setup did not finish")
            .hint("run `winapfs setup` in an Administrator terminal to see what went wrong"));
    }
    println!();
    println!("Done. winapfs starts with Windows and shows your Mac drive in Explorer whenever it is plugged in.");
    if disks::apfs_disks().is_empty() {
        println!("Plug in your Mac drive. After a few seconds it appears in Explorer (usually as M:).");
    } else {
        let l = launcher()?;
        let t = Instant::now();
        let mut shown = Vec::new();
        while shown.is_empty() && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(500));
            shown = l.running().unwrap_or_default().into_iter().filter(|(c, _)| c == CLASS).map(|(_, i)| i).collect();
        }
        match shown.first() {
            Some(d) => println!("Your Mac drive is in Explorer now, as {d}:."),
            None => println!("Your Mac drive should appear in Explorer within a few seconds."),
        }
    }
    println!("The `winapfs` command now works in any new terminal. To remove winapfs later, run: winapfs uninstall");
    Ok(())
}

/// `winapfs uninstall`
pub fn uninstall() -> CliResult {
    println!("Removing winapfs. Windows will ask for permission once.");
    if is_elevated() {
        setup(true)?;
    } else {
        run_elevated("setup --remove")?;
    }
    if registered() || automount_installed() {
        return Err(CliError::new("removal did not finish").hint("run `winapfs setup --remove` in an Administrator terminal"));
    }
    println!("winapfs is removed. WinFsp is still installed; remove it in Settings > Apps if you like.");
    Ok(())
}

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(key: isize, subkey: *const u16, options: u32, access: u32, out: *mut isize) -> i32;
    fn RegQueryValueExW(key: isize, name: *const u16, reserved: *mut u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32;
    fn RegSetValueExW(key: isize, name: *const u16, reserved: u32, kind: u32, data: *const u8, len: u32) -> i32;
    fn RegCloseKey(key: isize) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn SendMessageTimeoutW(hwnd: isize, msg: u32, wparam: usize, lparam: isize, flags: u32, timeout: u32, result: *mut usize) -> isize;
}

/// Adds the install folder to the system PATH, or removes it. Other entries
/// and the value's type (REG_EXPAND_SZ keeps `%SystemRoot%` references) stay
/// as they are; running programs such as Explorer are told about the change.
fn set_system_path(dir: &Path, add: bool) -> Result<(), String> {
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
    const KEY_QUERY_VALUE: u32 = 0x1;
    const KEY_SET_VALUE: u32 = 0x2;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const HWND_BROADCAST: isize = 0xffff;
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const SMTO_ABORTIFHUNG: u32 = 0x2;
    let sub = wide(r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
    let name = wide("Path");
    unsafe {
        let mut key = 0isize;
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_QUERY_VALUE | KEY_SET_VALUE, &mut key) != 0 {
            return Err("could not open the system PATH".into());
        }
        let (mut kind, mut len) = (0u32, 0u32);
        RegQueryValueExW(key, name.as_ptr(), std::ptr::null_mut(), &mut kind, std::ptr::null_mut(), &mut len);
        let mut buf = vec![0u16; len as usize / 2 + 1];
        let mut got = (buf.len() * 2) as u32;
        if RegQueryValueExW(key, name.as_ptr(), std::ptr::null_mut(), &mut kind, buf.as_mut_ptr().cast(), &mut got) != 0 {
            RegCloseKey(key);
            return Err("could not read the system PATH".into());
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let current = String::from_utf16_lossy(&buf[..end]);
        let target = dir.display().to_string();
        let same = |p: &str| p.trim_end_matches('\\').eq_ignore_ascii_case(target.trim_end_matches('\\'));
        let mut parts: Vec<&str> = current.split(';').filter(|p| !same(p)).collect();
        while parts.last() == Some(&"") {
            parts.pop();
        }
        if add {
            parts.push(&target);
        }
        let updated = parts.join(";");
        let mut result = Ok(());
        if updated != current {
            let data = wide(&updated);
            let kind = if kind == REG_SZ { REG_SZ } else { REG_EXPAND_SZ };
            if RegSetValueExW(key, name.as_ptr(), 0, kind, data.as_ptr().cast(), (data.len() * 2) as u32) != 0 {
                result = Err("could not update the system PATH".into());
            } else {
                let env = wide("Environment");
                let mut r = 0usize;
                SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 5000, &mut r);
            }
        }
        RegCloseKey(key);
        result
    }
}

/// Asks a yes/no question; Enter means yes. Without a keyboard (no input) it means no.
fn ask(question: &str) -> bool {
    print!("{question} [Y/n] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => !line.trim().to_ascii_lowercase().starts_with('n'),
    }
}

#[repr(C)]
struct ShellExecuteInfo {
    size: u32,
    mask: u32,
    hwnd: *mut c_void,
    verb: *const u16,
    file: *const u16,
    parameters: *const u16,
    directory: *const u16,
    show: i32,
    inst_app: *mut c_void,
    id_list: *mut c_void,
    class: *const u16,
    hkey_class: *mut c_void,
    hot_key: u32,
    icon_or_monitor: *mut c_void,
    process: *mut c_void,
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteExW(info: *mut ShellExecuteInfo) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
    fn GetLastError() -> u32;
}

/// Runs this program again with administrator rights (Windows shows its
/// permission prompt) and waits for it.
fn run_elevated(args: &str) -> CliResult {
    const SEE_MASK_NOCLOSEPROCESS: u32 = 0x40;
    const SW_HIDE: i32 = 0;
    const ERROR_CANCELLED: u32 = 1223;
    let exe = wide(&std::env::current_exe()?.display().to_string());
    let (verb, params) = (wide("runas"), wide(args));
    let mut info = ShellExecuteInfo {
        size: std::mem::size_of::<ShellExecuteInfo>() as u32,
        mask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: std::ptr::null_mut(),
        verb: verb.as_ptr(),
        file: exe.as_ptr(),
        parameters: params.as_ptr(),
        directory: std::ptr::null(),
        show: SW_HIDE,
        inst_app: std::ptr::null_mut(),
        id_list: std::ptr::null_mut(),
        class: std::ptr::null(),
        hkey_class: std::ptr::null_mut(),
        hot_key: 0,
        icon_or_monitor: std::ptr::null_mut(),
        process: std::ptr::null_mut(),
    };
    unsafe {
        if ShellExecuteExW(&mut info) == 0 {
            return Err(match GetLastError() {
                ERROR_CANCELLED => CliError::new("permission was not given").hint("run winapfs again and choose Yes"),
                e => CliError::new(format!("could not ask for administrator rights (Windows error {e})")),
            });
        }
        WaitForSingleObject(info.process, u32::MAX);
        let mut code = 1u32;
        GetExitCodeProcess(info.process, &mut code);
        CloseHandle(info.process);
        if code != 0 {
            return Err(CliError::new("the administrator step failed")
                .hint(format!("run `winapfs {args}` in an Administrator terminal to see why")));
        }
    }
    Ok(())
}

/// Whether this process has administrator rights right now.
pub fn is_elevated() -> bool {
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ELEVATION: u32 = 20;
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevated = 0u32;
        let mut ret = 0u32;
        let ok = GetTokenInformation(token, TOKEN_ELEVATION, (&mut elevated as *mut u32).cast(), 4, &mut ret);
        CloseHandle(token);
        ok != 0 && elevated != 0
    }
}

/// `winapfs mount <disk> <X:>` through the launcher.
pub fn mount(src: &str, mount_point: &str, vol: usize, part: Option<u32>, mode: MountMode) -> CliResult {
    let mut disk = disk_source(src).ok_or_else(|| CliError::new("the launcher can only mount whole disks (diskN)"))?;
    if disk == "auto" {
        // Resolve here, so the mount's command line names the disk (the auto-mount service relies on it).
        let n = *disks::apfs_disks().first().ok_or_else(|| {
            CliError::new("no connected disk has a Mac (APFS) partition").hint("is the Mac drive plugged in?")
        })?;
        disk = format!("disk{n}");
    }
    let inst = instance(mount_point);
    let root = format!("{inst}:\\");
    if Path::new(&root).exists() {
        return Err(CliError::new(format!("{inst}: is already in use")).hint("pick a free drive letter"));
    }
    let l = launcher()?;
    let mut args = vec![format!("src={disk}"), format!("at={inst}:"), format!("vol={vol}")];
    if let Some(p) = part {
        args.push(format!("part={p}"));
    }
    if let Some(m) = mode.arg() {
        args.push(m.to_string());
    }
    l.start(CLASS, &inst, &args).map_err(|e| CliError::new(e.to_string()))?;

    let log = install_dir().join("serve.log");
    let started = Instant::now();
    while !Path::new(&root).exists() {
        let gone = started.elapsed() > Duration::from_secs(2) && !running_here(&l, &inst);
        // Writable mounts check the whole space map first.
        let limit = if mode == MountMode::ReadOnly { 45 } else { 300 };
        if gone || started.elapsed() > Duration::from_secs(limit) {
            let _ = l.stop(CLASS, &inst);
            return Err(CliError::new(format!("winapfs could not mount {disk} at {inst}:"))
                .hint(format!("details in {}", log.display())));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("mounted   {disk} volume {vol} at {inst}:, {}, visible in Explorer", mode.describe());
    println!("stop      winapfs unmount {inst}:");
    println!("note      {WINFSP_NOTICE}");
    Ok(())
}

/// `winapfs unmount <X: or folder>`: launcher instances first, then a mount
/// running in this session.
pub fn unmount(mount_point: &str) -> CliResult {
    if mount_point.eq_ignore_ascii_case("all") {
        let l = launcher()?;
        let drives: Vec<String> = l
            .running()
            .map_err(|e| CliError::new(e.to_string()))?
            .into_iter()
            .filter(|(c, _)| c == CLASS)
            .map(|(_, i)| i)
            .collect();
        if drives.is_empty() {
            println!("no winapfs drive letters mounted");
        }
        for d in drives {
            unmount(&format!("{d}:"))?;
        }
        return Ok(());
    }
    if is_drive_letter(mount_point) {
        let inst = instance(mount_point);
        if let Ok(l) = Launcher::load() {
            if running_here(&l, &inst) {
                l.stop(CLASS, &inst).map_err(|e| CliError::new(e.to_string()))?;
                let root = format!("{inst}:\\");
                let t = Instant::now();
                while Path::new(&root).exists() && t.elapsed() < Duration::from_secs(15) {
                    std::thread::sleep(Duration::from_millis(200));
                }
                println!("unmounted {inst}:");
                return Ok(());
            }
        }
    }
    if !crate::mountfs::request_stop(mount_point) {
        return Err(CliError::new(format!("nothing is mounted at {mount_point} by winapfs"))
            .hint("`winapfs mounts` lists drives; folder mounts must be unmounted by the same user and elevation"));
    }
    println!("asked the mount at {mount_point} to stop");
    Ok(())
}

/// `winapfs mounts`
pub fn list() -> CliResult {
    let l = launcher()?;
    let drives: Vec<String> =
        l.running().map_err(|e| CliError::new(e.to_string()))?.into_iter().filter(|(c, _)| c == CLASS).map(|(_, i)| i).collect();
    if drives.is_empty() {
        println!("no winapfs drive letters mounted");
    } else {
        for d in &drives {
            println!("{d}:  (stop: winapfs unmount {d}:)");
        }
    }
    if std::env::var_os("WINAPFS_DEBUG").is_some() {
        for d in &drives {
            println!("  launcher info for {d}: {:?}", l.info(CLASS, d));
        }
    }
    let state = if l.is_registered(CLASS) { "yes" } else { "no; run `winapfs setup` once as administrator" };
    println!("set up with the WinFsp launcher: {state}");
    Ok(())
}

/// `winapfs serve src=diskN at=X: [vol=N] [part=N]`, run by the launcher as
/// SYSTEM. Any account the setup allowed can trigger it, so it accepts only
/// whole disks (not files the caller may be unable to read) and drive
/// letters (not folders, which would be created with SYSTEM's rights).
pub fn serve(args: &[String]) -> CliResult {
    let (mut src, mut at, mut vol, mut part, mut mode) = (None, None, 0usize, None, MountMode::ReadOnly);
    for a in args {
        match a.split_once('=') {
            Some(("src", v)) => src = Some(v.to_string()),
            Some(("at", v)) => at = Some(v.to_string()),
            Some(("vol", v)) => vol = v.parse().map_err(|_| CliError::new(format!("bad vol={v}")))?,
            Some(("part", v)) => part = Some(v.parse().map_err(|_| CliError::new(format!("bad part={v}")))?),
            Some(("mode", "write")) => mode = MountMode::Write,
            Some(("mode", "rehearse")) => mode = MountMode::Rehearse,
            _ => {} // the launcher may pass the instance name or empty placeholders
        }
    }
    let src = src.ok_or_else(|| CliError::new("serve needs src=diskN"))?;
    let at = at.ok_or_else(|| CliError::new("serve needs at=X:"))?;
    if !is_drive_letter(&at) {
        return Err(CliError::new(format!("serve mounts only at drive letters, not {at}")));
    }
    match src.as_str() {
        "demo" => cmd_mount(None, &at, 0, None, MountMode::ReadOnly),
        _ => {
            let disk = disk_source(&src).ok_or_else(|| CliError::new(format!("serve reads only whole disks, not {src}")))?;
            cmd_mount(Some(&disk), &at, vol, part, mode)
        }
    }
}

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(token: *mut c_void, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, out: *mut *mut u16) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn LocalFree(p: *mut c_void) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// The SID of the account running this process (with UAC, elevation keeps the same account).
fn current_user_sid() -> Result<String, String> {
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER: u32 = 1;
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err("could not read this process's account".into());
        }
        let mut buf = vec![0u64; 64]; // TOKEN_USER: a SID pointer, attributes, then the SID
        let mut ret = 0u32;
        let ok = GetTokenInformation(token, TOKEN_USER, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut ret);
        CloseHandle(token);
        if ok == 0 {
            return Err("could not read this process's account".into());
        }
        let sid = buf[0] as usize as *mut c_void;
        let mut s = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut s) == 0 {
            return Err("could not format the account SID".into());
        }
        let mut n = 0;
        while *s.add(n) != 0 {
            n += 1;
        }
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s.cast());
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_and_letters() {
        assert_eq!(disk_source("disk3").as_deref(), Some("disk3"));
        assert_eq!(disk_source(r"\\.\PhysicalDrive12").as_deref(), Some("disk12"));
        assert_eq!(disk_source(r"C:\images\mac.img"), None);
        assert!(is_drive_letter("Z:") && is_drive_letter("t:"));
        assert!(!is_drive_letter(r"C:\Mac") && !is_drive_letter("ZZ:") && !is_drive_letter("1:"));
    }

    #[test]
    fn sid_of_this_process() {
        assert!(current_user_sid().is_ok_and(|s| s.starts_with("S-1-5-")));
    }

    #[test]
    fn serve_rejects_files_and_folders() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(serve(&args(&["src=C:\\secret.img", "at=Z:"])).is_err());
        assert!(serve(&args(&["src=disk3", "at=C:\\Windows\\x"])).is_err());
        assert!(serve(&args(&["at=Z:"])).is_err());
    }
}
