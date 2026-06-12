//! Logging utilities: size-based rolling file appender and shared tracing timer.
//!
//! Used by both Gateway and Agent Runtime for consistent log file naming
//! (YYYYMMDD_HHMMSS.log) and auto-split behaviour.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A `FormatTime` implementation that uses `chrono::Local` to produce
/// RFC-3339 local-time timestamps (e.g. `2026-06-13T15:30:00.123+08:00`).
///
/// This avoids the `tracing-subscriber` `local-time` feature, which pulls
/// in the `time` crate and triggers an E0119 coherence conflict with
/// `EnvFilter`'s blanket `From<S: AsRef<str>>` impl.
#[derive(Default, Clone, Copy)]
pub struct ChronoLocalTimer;

impl std::fmt::Display for ChronoLocalTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", chrono::Local::now().to_rfc3339())
    }
}

impl tracing_subscriber::fmt::time::FormatTime for ChronoLocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z"))
    }
}

/// A file appender that auto-splits when the current log file exceeds a size limit.
/// Log files are named `YYYYMMDD_HHMMSS.log` using the creation timestamp.
pub struct SizeRollingFileAppender {
    dir: std::path::PathBuf,
    max_bytes: u64,
    max_file_count: AtomicUsize,
    inner: Mutex<AppenderInner>,
}

struct AppenderInner {
    file: std::fs::File,
    current_path: std::path::PathBuf,
    current_size: u64,
}

impl SizeRollingFileAppender {
    /// Create a new rolling file appender.
    ///
    /// `max_mb` — max file size in MB before rolling to a new file.
    /// `max_count` — maximum number of log files to keep (0 = unlimited).
    /// The initial file is named `YYYYMMDD_HHMMSS.log` based on current time.
    pub fn new(dir: std::path::PathBuf, max_mb: u64, max_count: usize) -> Self {
        let max_bytes = max_mb * 1024 * 1024;
        let now = chrono::Local::now();
        let filename = format!("{}.log", now.format("%Y%m%d_%H%M%S"));
        let path = dir.join(&filename);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap_or_else(|_| std::fs::File::create(&path).unwrap());
        let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        let appender = Self {
            dir,
            max_bytes,
            max_file_count: AtomicUsize::new(max_count),
            inner: Mutex::new(AppenderInner {
                file,
                current_path: path,
                current_size,
            }),
        };
        appender.enforce_max_file_count();
        appender
    }

    /// Create a new log file with a fresh timestamp name.
    fn roll(&self, inner: &mut AppenderInner) {
        let now = chrono::Local::now();
        let filename = format!("{}.log", now.format("%Y%m%d_%H%M%S"));
        let path = self.dir.join(&filename);
        match std::fs::File::create(&path) {
            Ok(file) => {
                inner.file = file;
                inner.current_path = path;
                inner.current_size = 0;
                // After rolling to a new file, enforce max file count
                let _ = inner;
                self.enforce_max_file_count();
            }
            Err(e) => {
                eprintln!("WARN: failed to create new log file {:?}: {}", path, e);
            }
        }
    }

    /// Enforce the maximum number of log files.
    /// When the number of `*.log` files exceeds `max_file_count`, delete the
    /// oldest files (sorted by filename, which is timestamp-based) to maintain
    /// the limit. No-op when `max_file_count == 0`.
    fn enforce_max_file_count(&self) {
        let max = self.max_file_count.load(Ordering::Relaxed);
        if max == 0 {
            return;
        }

        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };

        let mut log_files: Vec<std::path::PathBuf> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().is_some_and(|ext| ext == "log") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        if log_files.len() <= max {
            return;
        }

        // Sort by filename (YYYYMMDD_HHMMSS.log — lexicographic = chronological)
        log_files.sort();

        // Delete the oldest files, keeping the newest `max` files
        let to_remove = log_files.len() - max;
        for path in log_files.iter().take(to_remove) {
            if let Err(e) = std::fs::remove_file(path) {
                eprintln!("WARN: failed to delete old log file {:?}: {}", path, e);
            }
        }
    }

    /// Force immediate rotation: close current log file and open a new one.
    /// Called by the Runtime when Gateway requests log cleanup via IPC.
    /// The caller should delete old *.log files before calling this.
    pub fn force_rotate(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.roll(&mut inner);
    }

    /// Dynamically update the maximum number of log files to keep.
    /// Immediately enforces the new limit by deleting the oldest files
    /// when the current count exceeds the new maximum.
    pub fn set_max_file_count(&self, count: usize) {
        self.max_file_count.store(count, Ordering::Relaxed);
        self.enforce_max_file_count();
    }
}

impl Write for &SizeRollingFileAppender {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.current_size >= self.max_bytes {
            self.roll(&mut inner);
        }
        let n = inner.file.write(buf)?;
        inner.current_size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).file.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SizeRollingFileAppender {
    type Writer = &'a SizeRollingFileAppender;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }

    fn make_writer_for(&'a self, _meta: &tracing::Metadata<'_>) -> Self::Writer {
        self
    }
}
