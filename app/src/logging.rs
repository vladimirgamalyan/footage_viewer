//! File logging so a tester can send us a log after hitting a problem.
//!
//! The release build is a Windows GUI app (`windows_subsystem = "windows"`), so
//! there is no console and panics or errors would otherwise vanish. We write a
//! plain-text log next to the executable, keep the last few runs, and route
//! panics (with a backtrace) into it. Because eframe/wgpu log through the same
//! `log` facade, their diagnostics land in the file too.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{LevelFilter, Log, Metadata, Record};

/// Base name of the current run's log, written next to the executable.
const LOG_NAME: &str = "footage_viewer.log";

/// How many previous runs to keep alongside the current log. Each start shifts
/// the existing files up by one (`footage_viewer.log` -> `footage_viewer.1.log`
/// -> ...) and drops the oldest, so the folder never grows without bound.
const KEEP_PREVIOUS: usize = 4;

struct FileLogger {
    file: Mutex<File>,
}

impl Log for FileLogger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let line = format!(
            "{} {:<5} [{}] {}\n",
            utc_timestamp(),
            record.level(),
            record.target(),
            record.args()
        );
        // Write straight to the file (no buffering): the app logs sparingly, and
        // the global logger is leaked as `'static` so a buffer would never flush
        // on exit and would drop the last lines on a hard crash.
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
    }
}

/// Set up file logging and panic capture. Returns the path of the current log
/// so `main` can report where it lives. Never fails the app: if the file can't
/// be opened (e.g. a read-only folder), logging is silently skipped and the app
/// runs as before.
pub fn init() -> PathBuf {
    let dir = log_dir();
    rotate(&dir);
    let path = dir.join(LOG_NAME);

    if let Ok(file) = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        let logger = FileLogger {
            file: Mutex::new(file),
        };
        if log::set_boxed_logger(Box::new(logger)).is_ok() {
            // Info keeps our own events plus eframe/wgpu warnings and adapter
            // info, while dropping wgpu's very chatty debug/trace stream.
            log::set_max_level(LevelFilter::Info);
        }
    }

    install_panic_hook();
    path
}

/// Directory holding the executable, where the log is written. Falls back to the
/// current directory if the executable path can't be resolved.
fn log_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Path of the log file `n` runs back (`0` is the current `footage_viewer.log`).
fn log_path(dir: &Path, n: usize) -> PathBuf {
    if n == 0 {
        dir.join(LOG_NAME)
    } else {
        dir.join(format!("footage_viewer.{n}.log"))
    }
}

/// Drop the oldest kept log, then shift every remaining file up by one so the
/// current log is free to be recreated. Best-effort: missing files are fine and
/// rename failures just mean that slot keeps its old contents.
fn rotate(dir: &Path) {
    let _ = fs::remove_file(log_path(dir, KEEP_PREVIOUS));
    for n in (0..KEEP_PREVIOUS).rev() {
        let _ = fs::rename(log_path(dir, n), log_path(dir, n + 1));
    }
}

/// Log panics (message, location, backtrace) before the process dies, then run
/// the default hook so debug builds still print to the console.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_owned());
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!(
            "panic at {location}: {}\n{backtrace}",
            payload_str(info.payload())
        );
        log::logger().flush();
        default(info);
    }));
}

/// Best-effort human-readable form of a panic payload.
fn payload_str(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

/// Current UTC time as `YYYY-MM-DD HH:MM:SS.mmmZ`. Hand-rolled so logging pulls
/// in no date/time dependency; UTC keeps it unambiguous across machines.
fn utc_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}.{millis:03}Z")
}

/// Days since 1970-01-01 to `(year, month, day)` in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1)); // Unix epoch
        assert_eq!(civil_from_days(-1), (1969, 12, 31)); // day before
        assert_eq!(civil_from_days(11_017), (2000, 3, 1)); // leap-year boundary
        assert_eq!(civil_from_days(20_283), (2025, 7, 14));
    }

    #[test]
    fn log_paths_number_previous_runs() {
        let dir = Path::new("logs");
        assert_eq!(log_path(dir, 0), dir.join("footage_viewer.log"));
        assert_eq!(log_path(dir, 1), dir.join("footage_viewer.1.log"));
        assert_eq!(log_path(dir, 4), dir.join("footage_viewer.4.log"));
    }
}
