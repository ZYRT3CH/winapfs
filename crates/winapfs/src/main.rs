//! winapfs: read Mac (APFS) drives on Windows.

#[cfg(windows)]
mod automount;
#[cfg(windows)]
mod backends;
#[cfg(windows)]
mod disks;
mod dump;
#[cfg(windows)]
mod mountfs;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod winfsp;

use std::collections::HashSet;
use std::fs::{self, File, FileTimes};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use apfs::check::{check_volume, CheckOptions};
use apfs::{format_uuid, gpt, Container, FileDevice, FileKind, Inode, Volume};

const USAGE: &str = r"winapfs - read Mac (APFS) drives on Windows.
The disk is opened without write access, so nothing on it can change.

usage:
  winapfs info <source>
  winapfs ls   <source> [path] [--vol N]
  winapfs get  <source> <path> <dest-folder> [--vol N] [--all] [--dry-run]
  winapfs cat  <source> <path> [--vol N]
  winapfs verify <source> [--vol N]      check the whole volume reads correctly
  winapfs mount  <source> <X: or new-folder> [--vol N]   show the volume in Windows, read-only
  winapfs unmount <X: or folder or all>
  winapfs mounts                         list mounted drive letters
  winapfs disks                          list connected disks with a Mac (APFS) partition
  winapfs install                        set everything up (asks for permission once); Mac drives
                                         then appear in Explorer automatically, also after restarts
  winapfs uninstall                      remove what install set up
  winapfs setup [--remove]               the administrator part of install/uninstall
  winapfs mount-demo <X: or new-folder>  mount a built-in test tree (no Mac drive needed)

  <source>   disk3 (= \\.\PhysicalDrive3), auto (the first disk with an APFS partition),
             or a raw disk/container image file
  --vol N    volume index shown by `info` (default 0)
  --part N   GPT partition number (default: the first APFS partition)
  --all      also copy macOS housekeeping folders (.Spotlight-V100, .fseventsd, ...)
  --dry-run  show what `get` would copy and rename, without writing anything
  --here     mount inside this terminal instead of through the launcher

Raw disks can only be opened from an Administrator terminal.
Mounting uses WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
https://github.com/winfsp/winfsp";

#[cfg(windows)]
const WINFSP_NOTICE: &str =
    "uses WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos - https://github.com/winfsp/winfsp";

/// macOS bookkeeping that is useless on Windows; skipped by `get` unless --all.
/// `Icon\r` is the hidden file Finder uses for a folder's custom icon.
const MAC_HOUSEKEEPING: &[&str] = &[
    ".Spotlight-V100",
    ".fseventsd",
    ".Trashes",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    ".DS_Store",
    "Icon\r",
];

struct CliError {
    msg: String,
    hint: Option<String>,
}

impl CliError {
    fn new(msg: impl Into<String>) -> Self {
        Self { msg: msg.into(), hint: None }
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl From<apfs::Error> for CliError {
    fn from(e: apfs::Error) -> Self {
        let hint = match &e {
            apfs::Error::Unsupported(_) => Some("this needs a later winapfs milestone; please report what you hit".into()),
            apfs::Error::NotFound(_) => Some("run `winapfs ls` on the parent folder to see the exact names".into()),
            _ => None,
        };
        Self { msg: e.to_string(), hint }
    }
}

impl From<io::Error> for CliError {
    fn from(e: io::Error) -> Self {
        Self::new(e.to_string())
    }
}

type CliResult<T = ()> = Result<T, CliError>;

struct Args {
    cmd: String,
    pos: Vec<String>,
    vol: usize,
    part: Option<u32>,
    all: bool,
    dry_run: bool,
    here: bool,
}

fn parse_args() -> CliResult<Args> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or_else(|| CliError::new("missing command").hint(USAGE))?;
    let cmd = match cmd.as_str() {
        "-V" | "--version" => "version".to_string(),
        "-h" | "--help" => "help".to_string(),
        _ => cmd,
    };
    let mut a = Args { cmd, pos: Vec::new(), vol: 0, part: None, all: false, dry_run: false, here: false };
    while let Some(arg) = it.next() {
        let mut num = |flag: &str| -> CliResult<u64> {
            it.next()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| CliError::new(format!("{flag} needs a number")))
        };
        match arg.as_str() {
            "--vol" | "-v" => a.vol = num("--vol")? as usize,
            "--part" => a.part = Some(num("--part")? as u32),
            "--all" => a.all = true,
            "--dry-run" => a.dry_run = true,
            "--here" => a.here = true,
            "-h" | "--help" => a.cmd = "help".into(),
            "-V" | "--version" => a.cmd = "version".into(),
            _ => a.pos.push(arg),
        }
    }
    Ok(a)
}

fn main() -> ExitCode {
    // Double-clicked in Explorer: no arguments, and the console window is ours alone.
    #[cfg(windows)]
    let double_clicked = std::env::args().len() == 1 && owns_console();
    #[cfg(not(windows))]
    let double_clicked = false;
    let result = if double_clicked { service::install() } else { parse_args().and_then(|a| run(&a)) };
    let code = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", e.msg);
            if let Some(h) = e.hint {
                eprintln!("hint: {h}");
            }
            ExitCode::FAILURE
        }
    };
    if double_clicked {
        // Keep the window open long enough to read.
        print!("\nPress Enter to close this window.");
        let _ = io::stdout().flush();
        let _ = io::stdin().read_line(&mut String::new());
    }
    code
}

/// True when no other process (such as a shell) shares this console, which
/// is how a program started by double-clicking it finds itself.
#[cfg(windows)]
fn owns_console() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }
    let mut ids = [0u32; 4];
    unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) == 1 }
}

fn run(a: &Args) -> CliResult {
    let need = |n: usize| -> CliResult {
        if a.pos.len() < n {
            return Err(CliError::new(format!("`{}` needs {n} argument(s)", a.cmd)).hint(USAGE));
        }
        Ok(())
    };
    match a.cmd.as_str() {
        "help" => {
            println!("{USAGE}");
            Ok(())
        }
        "version" => {
            println!("winapfs {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "info" => {
            need(1)?;
            cmd_info(&a.pos[0], a.part)
        }
        "ls" => {
            need(1)?;
            let path = a.pos.get(1).map_or("/", String::as_str);
            cmd_ls(&a.pos[0], path, a.vol, a.part)
        }
        "get" => {
            need(3)?;
            cmd_get(&a.pos[0], &a.pos[1], Path::new(&a.pos[2]), a.vol, a.part, a.all, a.dry_run)
        }
        "verify" => {
            need(1)?;
            cmd_verify(&a.pos[0], a.vol, a.part)
        }
        #[cfg(windows)]
        "mount" => {
            need(2)?;
            let (src, at) = (&a.pos[0], &a.pos[1]);
            // Whole disk at a drive letter: go through the launcher when set up, so Explorer sees it.
            let disk_at_letter = service::is_drive_letter(at) && service::disk_source(src).is_some();
            if !a.here && disk_at_letter && service::registered() {
                service::mount(src, at, a.vol, a.part)
            } else {
                cmd_mount(Some(src), at, a.vol, a.part).map_err(|e| match e.msg.starts_with("access denied") {
                    true if disk_at_letter => e.hint(
                        "run `winapfs setup` once from an Administrator terminal; after that this works from a normal terminal",
                    ),
                    _ => e,
                })
            }
        }
        #[cfg(windows)]
        "mount-demo" => {
            need(1)?;
            cmd_mount(None, &a.pos[0], 0, None)
        }
        #[cfg(windows)]
        "unmount" => {
            need(1)?;
            service::unmount(&a.pos[0])
        }
        #[cfg(windows)]
        "mounts" => service::list(),
        #[cfg(windows)]
        "disks" => {
            let found = disks::apfs_disks();
            if found.is_empty() {
                println!("no connected disk has a Mac (APFS) partition");
            }
            for n in &found {
                println!("disk{n}  has a Mac (APFS) partition");
            }
            if let Some(n) = found.first() {
                println!("next: winapfs mount disk{n} M:");
            }
            Ok(())
        }
        #[cfg(windows)]
        "setup" => service::setup(a.pos.iter().any(|p| p == "--remove" || p == "remove")),
        #[cfg(windows)]
        "install" => service::install(),
        #[cfg(windows)]
        "uninstall" => service::uninstall(),
        #[cfg(windows)]
        "automount-run" => automount::run_in_terminal(),
        #[cfg(windows)]
        "automount-service" => automount::run_as_service(),
        #[cfg(windows)]
        "serve" => service::serve(&a.pos),
        "cat" => {
            need(2)?;
            cmd_cat(&a.pos[0], &a.pos[1], a.vol, a.part)
        }
        "dump" => {
            need(1)?;
            dump::dump(&a.pos[0], a.part)
        }
        // Developer commands for write support; image files only.
        "dev-commit" => {
            need(1)?;
            let n: u32 = a.pos.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            let mut w = apfs::write::Writer::open(Path::new(&a.pos[0]))?;
            for _ in 0..n {
                println!("committed xid {}", w.commit_empty()?);
            }
            Ok(())
        }
        other => Err(CliError::new(format!("unknown command `{other}`")).hint(USAGE)),
    }
}

/// `disk3`, `disk:3` and `PhysicalDrive3` all mean `\\.\PhysicalDrive3`.
fn source_path(src: &str) -> String {
    let lower = src.to_ascii_lowercase();
    let digits = lower
        .strip_prefix("disk")
        .or_else(|| lower.strip_prefix("physicaldrive"))
        .map(|d| d.trim_start_matches(':'));
    match digits {
        Some(d) if !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) => format!(r"\\.\PhysicalDrive{d}"),
        _ => src.to_string(),
    }
}

/// The first physical disk with an APFS partition, as `diskN`.
fn find_apfs_disk() -> CliResult<String> {
    #[cfg(windows)]
    if let Some(n) = disks::apfs_disks().first() {
        return Ok(format!("disk{n}"));
    }
    Err(CliError::new("no connected disk has an APFS partition").hint("is the Mac drive plugged in?"))
}

fn open_source(src: &str, part: Option<u32>) -> CliResult<(Container, String)> {
    let found;
    let src = if src.eq_ignore_ascii_case("auto") {
        found = find_apfs_disk()?;
        found.as_str()
    } else {
        src
    };
    let path = source_path(src);
    let dev = FileDevice::open(Path::new(&path)).map_err(|e| match &e {
        apfs::Error::Io(io) if io.kind() == io::ErrorKind::PermissionDenied => {
            CliError::new(format!("access denied to {path}"))
                .hint("raw disks need an Administrator terminal (right-click Terminal > Run as administrator)")
        }
        apfs::Error::Io(io) if io.kind() == io::ErrorKind::NotFound => {
            CliError::new(format!("{path} does not exist")).hint("check the disk number in Disk Management")
        }
        _ => e.into(),
    })?;
    let (dev, location) = match gpt::read_partitions(&dev)? {
        None => (dev, path.clone()),
        Some(parts) => {
            let chosen = match part {
                Some(n) => parts.iter().find(|p| p.number == n),
                None => parts.iter().find(|p| p.is_apfs()),
            };
            let Some(p) = chosen else {
                let list: Vec<String> = parts
                    .iter()
                    .map(|p| format!("{}{}", p.number, if p.is_apfs() { " (APFS)" } else { "" }))
                    .collect();
                return Err(CliError::new(format!("no matching APFS partition on {path}"))
                    .hint(format!("partitions: {}", list.join(", "))));
            };
            (dev.with_base(p.offset)?, format!("{path} partition {}", p.number))
        }
    };
    Ok((Container::open(Box::new(dev))?, location))
}

fn cmd_info(src: &str, part: Option<u32>) -> CliResult {
    let (c, location) = open_source(src, part)?;
    let bs = c.block_size() as u64;
    println!("source     {location}");
    println!(
        "container  {}  block {}  xid {}  uuid {}",
        human_size(c.sb.block_count * bs),
        bs,
        c.xid(),
        format_uuid(&c.sb.uuid)
    );
    let vols = c.volume_superblocks()?;
    println!("volumes    {}", vols.len());
    for (i, v) in vols.iter().enumerate() {
        let enc = if v.is_encrypted() { "ENCRYPTED (not supported yet)" } else { "unencrypted" };
        let case = if v.is_case_insensitive() { "case-insensitive" } else { "case-sensitive" };
        println!(
            "  [{i}] \"{}\"  role {}  {case}  {enc}  files {}  folders {}  used {}",
            v.name,
            v.role_name(),
            v.num_files,
            v.num_directories,
            human_size(v.alloc_blocks * bs)
        );
        println!("      last written by {}  at {}", v.last_modified_by, format_time(v.last_mod_time));
    }
    if let Some((i, _)) = vols.iter().enumerate().find(|(_, v)| !v.is_encrypted()) {
        println!("next: winapfs ls {src} / --vol {i}");
    }
    Ok(())
}

fn cmd_ls(src: &str, path: &str, vol: usize, part: Option<u32>) -> CliResult {
    let (c, _) = open_source(src, part)?;
    let v = c.volume(vol)?;
    let target = v.lookup_path(path)?;
    if target.kind() != FileKind::Directory {
        print_entry(&v, &target, path)?;
        return Ok(());
    }
    let mut entries = v.read_dir(target.id)?;
    entries.sort_by_key(|e| e.name.to_lowercase());
    println!("{path}  {} entries  (volume {vol} \"{}\")", entries.len(), v.sb.name);
    if entries.is_empty() {
        println!("(empty folder)");
        return Ok(());
    }
    for e in &entries {
        match v.inode(e.file_id) {
            Ok(inode) => print_entry(&v, &inode, &e.name)?,
            Err(err) => println!("?     {:>16}  {:>10}  {}  ({err})", "", "", e.name),
        }
    }
    let first_dir = entries.iter().find(|e| e.kind == FileKind::Directory);
    let example = first_dir.map_or_else(|| path.to_string(), |d| join_path(path, &d.name));
    println!("next: winapfs get {src} \"{example}\" <dest-folder> --vol {vol}");
    Ok(())
}

fn print_entry(v: &Volume, inode: &Inode, name: &str) -> CliResult {
    let (kind, size, suffix) = match inode.kind() {
        FileKind::Directory => ("dir ", "-".to_string(), "/".to_string()),
        FileKind::Regular => ("file", human_size(v.file_size(inode)?), String::new()),
        FileKind::Symlink => ("link", "-".to_string(), format!(" -> {}", v.read_link(inode)?)),
        _ => ("misc", "-".to_string(), String::new()),
    };
    println!("{kind}  {}  {size:>10}  {name}{suffix}", format_time(inode.mod_time));
    Ok(())
}

#[derive(Default)]
struct CopyStats {
    files: u64,
    dirs: u64,
    bytes: u64,
    renamed: u64,
    rename_examples: Vec<String>,
    skipped: Vec<String>,
    errors: Vec<String>,
    last_report: Option<Instant>,
}

impl CopyStats {
    fn progress(&mut self) {
        if io::stderr().is_terminal() && self.last_report.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            eprint!("\rcopied {} files, {}   ", self.files, human_size(self.bytes));
            self.last_report = Some(Instant::now());
        }
    }
}

fn cmd_get(src: &str, path: &str, dest: &Path, vol: usize, part: Option<u32>, all: bool, dry: bool) -> CliResult {
    let (c, _) = open_source(src, part)?;
    let v = c.volume(vol)?;
    let target = v.lookup_path(path)?;
    if !dry {
        fs::create_dir_all(dest)?;
    }
    let name = path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .map_or_else(|| v.sb.name.clone(), str::to_string);
    if !dry {
        // Size the selection first (a metadata-only walk) and refuse to fill the destination drive.
        let mut planned = CopyStats::default();
        copy_entry(&v, &target, &name, dest, &mut HashSet::new(), &mut planned, all, true)?;
        if let Some(free) = free_space(dest) {
            if planned.bytes + HEADROOM > free {
                return Err(CliError::new(format!(
                    "{} to copy, but only {} free at {}",
                    human_size(planned.bytes),
                    human_size(free),
                    dest.display()
                ))
                .hint(format!("free up space or pick another destination (winapfs keeps {} spare)", human_size(HEADROOM))));
            }
        }
    }
    let mut stats = CopyStats::default();
    copy_entry(&v, &target, &name, dest, &mut HashSet::new(), &mut stats, all, dry)?;
    if stats.last_report.is_some() {
        eprintln!();
    }

    println!(
        "{} {} files, {} folders, {} into {}",
        if dry { "would copy" } else { "copied" },
        stats.files,
        stats.dirs,
        human_size(stats.bytes),
        dest.display()
    );
    if stats.renamed > 0 {
        println!("renamed {} item(s) whose Mac names are not allowed on Windows, e.g.", stats.renamed);
        for r in &stats.rename_examples {
            println!("  {r}");
        }
    }
    for s in &stats.skipped {
        println!("skipped {s}");
    }
    for e in &stats.errors {
        println!("FAILED  {e}");
    }
    if !stats.errors.is_empty() {
        return Err(CliError::new(format!("{} item(s) could not be copied", stats.errors.len())));
    }
    Ok(())
}

fn copy_entry(
    v: &Volume,
    inode: &Inode,
    name: &str,
    dest_dir: &Path,
    used: &mut HashSet<String>,
    stats: &mut CopyStats,
    all: bool,
    dry: bool,
) -> CliResult {
    let safe = unique_name(windows_safe(name), used);
    if safe != name {
        stats.renamed += 1;
        if stats.rename_examples.len() < 10 {
            stats.rename_examples.push(format!("{name}  ->  {safe}"));
        }
    }
    let target = dest_dir.join(&safe);
    match inode.kind() {
        FileKind::Directory => {
            if !dry {
                fs::create_dir_all(&target)?;
            }
            stats.dirs += 1;
            let mut child_used = HashSet::new();
            for e in v.read_dir(inode.id)? {
                if !all && MAC_HOUSEKEEPING.contains(&e.name.as_str()) {
                    continue;
                }
                let result = v
                    .inode(e.file_id)
                    .map_err(CliError::from)
                    .and_then(|child| copy_entry(v, &child, &e.name, &target, &mut child_used, stats, all, dry));
                if let Err(err) = result {
                    stats.errors.push(format!("{}: {}", target.join(&e.name).display(), err.msg));
                }
            }
        }
        FileKind::Regular if dry => {
            stats.files += 1;
            stats.bytes += v.file_size(inode)?;
        }
        FileKind::Regular => {
            if let Err(err) = copy_file(v, inode, &target, stats) {
                let _ = fs::remove_file(&target); // never leave a truncated file behind
                return Err(err);
            }
        }
        FileKind::Symlink => {
            let to = v.read_link(inode).unwrap_or_default();
            stats.skipped.push(format!("{} (symlink -> {to})", target.display()));
        }
        other => stats.skipped.push(format!("{} ({other:?})", target.display())),
    }
    Ok(())
}

fn copy_file(v: &Volume, inode: &Inode, target: &PathBuf, stats: &mut CopyStats) -> CliResult {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(target)?);
    let n = v.read_file(inode, &mut w)?;
    let f = w.into_inner().map_err(|e| CliError::from(e.into_error()))?;
    let mut times = FileTimes::new().set_modified(to_system_time(inode.mod_time)).set_accessed(to_system_time(inode.access_time));
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTimesExt;
        times = times.set_created(to_system_time(inode.create_time));
    }
    f.set_times(times)?;
    stats.files += 1;
    stats.bytes += n;
    stats.progress();
    Ok(())
}

fn cmd_verify(src: &str, vol: usize, part: Option<u32>) -> CliResult {
    let (c, location) = open_source(src, part)?;
    let v = c.volume(vol)?;
    println!("verify    volume {vol} \"{}\" on {location}", v.sb.name);
    let started = Instant::now();
    let mut last = Instant::now();
    let live = io::stderr().is_terminal();
    let rep = check_volume(&v, &CheckOptions::default(), &mut |msg| {
        if live && last.elapsed() > Duration::from_millis(500) {
            eprint!("\r{msg:<60}");
            last = Instant::now();
        }
    })?;
    if live {
        eprint!("\r{:<60}\r", "");
    }

    let sb = &v.sb;
    println!("metadata  {} records, {} tree nodes read, every checksum verified", rep.records, c.node_reads());
    for (label, found, expected) in [
        ("files", rep.files, sb.num_files),
        ("folders", rep.dirs, sb.num_directories),
        ("symlinks", rep.symlinks, sb.num_symlinks),
        ("other", rep.other, sb.num_other_fsobjects),
    ] {
        let verdict = if found == expected { "ok".to_string() } else { format!("differs by {}", found as i64 - expected as i64) };
        println!("  {label:<9} {found:>9}  superblock says {expected:>9}  {verdict}");
    }
    println!(
        "  dir entries {}  xattrs {}  extents {}  compressed files {}",
        rep.dir_entries, rep.xattrs, rep.extents, rep.compressed
    );
    println!("data      {} read", human_size(rep.data_bytes));
    for f in &rep.formats {
        if f.checked == 0 {
            println!("  {:<14} none on this volume", f.format);
        } else if f.mislabelled > 0 {
            println!(
                "  {:<14} {}/{} structurally intact, {} mislabelled (see notes)",
                f.format, f.ok, f.checked, f.mislabelled
            );
        } else {
            println!("  {:<14} {}/{} structurally intact", f.format, f.ok, f.checked);
        }
    }
    println!("  compressed     {}/{} decompressed to their exact size", rep.compressed_ok, rep.compressed_checked);
    println!("  {} files read via both copy paths and identical", rep.compared);
    if !rep.notes.is_empty() {
        println!("notes");
        for n in &rep.notes {
            println!("  {n}");
        }
    }
    if rep.problems.count == 0 {
        println!("problems  none");
    } else {
        println!("problems  {}", rep.problems.count);
        for p in &rep.problems.examples {
            println!("  {p}");
        }
    }
    println!("time      {:.1}s", started.elapsed().as_secs_f64());
    if rep.problems.count > 0 {
        return Err(CliError::new(format!("{} problem(s) found", rep.problems.count)));
    }
    Ok(())
}

/// Mounts an APFS volume (`src`) or, with `src == None`, the built-in demo tree.
#[cfg(windows)]
fn cmd_mount(src: Option<&str>, mount_point: &str, vol: usize, part: Option<u32>) -> CliResult {
    let is_drive = mount_point.len() == 2 && mount_point.ends_with(':');
    if !is_drive && Path::new(mount_point).exists() {
        return Err(CliError::new(format!("{mount_point} already exists"))
            .hint("WinFsp creates the mount folder itself; pick a path that does not exist yet"));
    }
    let (backend, what): (&'static dyn mountfs::Backend, String) = match src {
        Some(src) => {
            let (c, location) = open_source(src, part)?;
            let c: &'static Container = Box::leak(Box::new(c));
            let b = backends::ApfsBackend::new(c, vol)?;
            let what = format!("volume {vol} \"{}\" from {location}", mountfs::Backend::label(&b));
            (Box::leak(Box::new(b)), what)
        }
        None => (Box::leak(Box::new(backends::DemoBackend::new())), "the built-in demo tree".to_string()),
    };
    let mounted = mountfs::mount(backend, mount_point).map_err(CliError::new)?;
    println!("mounted   {what} at {mount_point}, read-only");
    println!("stop      Ctrl+C here, or: winapfs unmount {mount_point}");
    println!("note      {WINFSP_NOTICE}");
    if is_drive && src.is_some_and(|s| service::disk_source(s).is_some()) && !service::registered() {
        println!("tip       drive letters mounted from an Administrator terminal are hidden from Explorer;");
        println!("          run `winapfs setup` once as administrator to fix that");
    }
    let _ = io::stdout().flush();
    mountfs::wait_for_stop(mount_point).map_err(CliError::new)?;
    mounted.stop();
    println!("unmounted {mount_point}");
    let _ = io::stdout().flush();
    mountfs::signal_unmounted();
    Ok(())
}

fn cmd_cat(src: &str, path: &str, vol: usize, part: Option<u32>) -> CliResult {
    let (c, _) = open_source(src, part)?;
    let v = c.volume(vol)?;
    let target = v.lookup_path(path)?;
    if target.kind() != FileKind::Regular {
        return Err(CliError::new(format!("{path} is not a regular file")));
    }
    let mut out = BufWriter::new(io::stdout().lock());
    v.read_file(&target, &mut out)?;
    out.flush()?;
    Ok(())
}

/// Mac names may contain characters, endings and device names Windows rejects.
fn windows_safe(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .collect();
    // Windows drops trailing dots and spaces itself; do it up front so the name is predictable.
    s.truncate(s.trim_end_matches(['.', ' ']).len());
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let numbered = |p: &str| {
        stem.len() == 4 && stem.starts_with(p) && matches!(stem.as_bytes()[3], b'1'..=b'9')
    };
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered("COM") || numbered("LPT") {
        s.insert(0, '_');
    }
    if s.is_empty() {
        s.push('_');
    }
    s
}

/// Windows names are case-insensitive; a case-sensitive Mac volume can hold
/// both `Photo.jpg` and `photo.jpg` in one folder.
fn unique_name(name: String, used: &mut HashSet<String>) -> String {
    if used.insert(name.to_lowercase()) {
        return name;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name.as_str(), ""),
    };
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|candidate| used.insert(candidate.to_lowercase()))
        .unwrap()
}

/// Space `get` leaves free on the destination drive.
const HEADROOM: u64 = 1 << 30;

/// Bytes available to this user on the drive holding `dir`.
#[cfg(windows)]
fn free_space(dir: &Path) -> Option<u64> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut avail = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(avail)
}

#[cfg(not(windows))]
fn free_space(_dir: &Path) -> Option<u64> {
    None
}

fn join_path(parent: &str, name: &str) -> String {
    format!("{}/{name}", parent.trim_end_matches('/'))
}

fn to_system_time(ns: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns)
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// `YYYY-MM-DD HH:MM` in UTC from nanoseconds since 1970.
fn format_time(ns: u64) -> String {
    let secs = ns / 1_000_000_000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_sources() {
        assert_eq!(source_path("disk3"), r"\\.\PhysicalDrive3");
        assert_eq!(source_path("Disk:12"), r"\\.\PhysicalDrive12");
        assert_eq!(source_path("PhysicalDrive0"), r"\\.\PhysicalDrive0");
        assert_eq!(source_path("disk.img"), "disk.img");
    }

    #[test]
    fn windows_names() {
        assert_eq!(windows_safe("a:b?.txt"), "a_b_.txt");
        assert_eq!(windows_safe("CON.txt"), "_CON.txt");
        assert_eq!(windows_safe("COM0"), "COM0");
        assert_eq!(windows_safe("notes."), "notes");
        assert_eq!(windows_safe("dust osv "), "dust osv");
        assert_eq!(windows_safe("..."), "_");
        assert_eq!(windows_safe("Icon\r"), "Icon_");
        let mut used = HashSet::new();
        assert_eq!(unique_name("Photo.jpg".into(), &mut used), "Photo.jpg");
        assert_eq!(unique_name("photo.jpg".into(), &mut used), "photo (2).jpg");
    }

    #[cfg(windows)]
    #[test]
    fn free_space_of_current_drive() {
        assert!(free_space(Path::new(".")).is_some_and(|b| b > 0));
        assert!(free_space(Path::new("Q:\\no\\such\\drive")).is_none());
    }

    #[test]
    fn times() {
        assert_eq!(format_time(0), "1970-01-01 00:00");
        assert_eq!(format_time(1_700_000_000 * 1_000_000_000), "2023-11-14 22:13");
    }
}
