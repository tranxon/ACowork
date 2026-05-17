//! Logging utilities: size-based rolling file appender.
//!
//! Used by both Gateway and Agent Runtime for consistent log file naming
//! (YYYYMMDD_HHMMSS.log) and auto-split behaviour.

use std::io::Write;
use std::sync::Mutex;

/// A file appender that auto-splits when the current log file exceeds a size limit.
/// Log files are named `YYYYMMDD_HHMMSS.log` using the creation timestamp.
pub struct SizeRollingFileAppender {
    dir: std::path::PathBuf,
    max_bytes: u64,
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
    /// The initial file is named `YYYYMMDD_HHMMSS.log` based on current time.
    pub fn new(dir: std::path::PathBuf, max_mb: u64) -> Self {
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

        Self {
            dir,
            max_bytes,
            inner: Mutex::new(AppenderInner {
                file,
                current_path: path,
                current_size,
            }),
        }
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
            }
            Err(e) => {
                eprintln!("WARN: failed to create new log file {:?}: {}", path, e);
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
