//! Timing diagnostics. Slow operations are always reported on standard
//! error; with `WINAPFS_TRACE` set, or a file named `trace.on` in
//! `%ProgramData%\winapfs`, every one is, and messages also go to
//! `%ProgramData%\winapfs\trace.log` (readable even when the launcher runs
//! winapfs in the background as SYSTEM).

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static ON: OnceLock<bool> = OnceLock::new();

fn dir() -> PathBuf {
    std::env::var_os("ProgramData").map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from).join("winapfs")
}

pub fn on() -> bool {
    *ON.get_or_init(|| std::env::var_os("WINAPFS_TRACE").is_some() || dir().join("trace.on").exists())
}

/// Hooks the library's timing messages up to [`log`].
pub fn init() {
    apfs::write::set_trace(on());
    apfs::write::set_log(log);
}

pub fn log(msg: &str) {
    eprintln!("{msg}");
    if on() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir().join("trace.log")) {
            let _ = writeln!(f, "{stamp:.3} {msg}");
        }
    }
}

/// Reports `what` if it took long (or always, when tracing).
pub fn slow(what: &str, start: Instant) {
    let took = start.elapsed();
    if took > Duration::from_millis(200) || (on() && took > Duration::from_millis(5)) {
        log(&format!("winapfs: {what} took {:.3} s", took.as_secs_f64()));
    }
}
