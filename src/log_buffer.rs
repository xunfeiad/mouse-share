//! A ring-buffer `log::Log` implementation so the GUI can render the recent
//! log stream in its Log tab. It also forwards every record to an inner
//! `env_logger` so the terminal output is unchanged.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

/// How many records to keep in memory. ~2 KB per line × 1000 ≈ 2 MB worst
/// case, which is fine for a long-running desktop session.
const CAPACITY: usize = 1000;

#[derive(Clone, Debug)]
pub struct LogLine {
    /// Unix millis when the record was captured.
    pub ts_ms: u64,
    pub level: Level,
    pub message: String,
}

/// In-memory ring buffer. Cloneable (`Arc` internally) so both the UI and
/// the logger can hold references.
#[derive(Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<LogLine>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY))),
        }
    }

    pub fn push(&self, line: LogLine) {
        let mut buf = self.inner.lock().unwrap();
        if buf.len() == CAPACITY {
            buf.pop_front();
        }
        buf.push_back(line);
    }

    /// Snapshot the current buffer as a `Vec`. Called by the UI on each
    /// repaint — cost is linear in buffer length, which is capped.
    pub fn snapshot(&self) -> Vec<LogLine> {
        let buf = self.inner.lock().unwrap();
        buf.iter().cloned().collect()
    }
}

/// A `log::Log` implementation that writes to both `env_logger` (for
/// stderr) and an in-memory ring buffer (for the GUI).
struct TeeLogger {
    inner: env_logger::Logger,
    buffer: LogBuffer,
}

impl Log for TeeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        self.inner.log(record);
        self.buffer.push(LogLine {
            ts_ms: now_ms(),
            level: record.level(),
            message: record.args().to_string(),
        });
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Global singleton so the UI can access the shared log buffer without
/// plumbing it through every callsite.
static GLOBAL: OnceLock<LogBuffer> = OnceLock::new();

/// Install the tee logger as the global `log` backend. Safe to call more
/// than once — subsequent calls are no-ops. Returns the shared ring buffer.
pub fn install() -> LogBuffer {
    if let Some(existing) = GLOBAL.get() {
        return existing.clone();
    }

    let buffer = LogBuffer::new();
    let inner = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .format_timestamp_millis()
    .build();
    let filter = inner.filter();

    let tee = TeeLogger {
        inner,
        buffer: buffer.clone(),
    };

    if log::set_boxed_logger(Box::new(tee)).is_ok() {
        log::set_max_level(filter);
    } else {
        // Another logger was installed before us — fall back to the
        // buffer-only logger without the env_logger forward.
        log::set_max_level(LevelFilter::Info);
    }

    let _ = GLOBAL.set(buffer.clone());
    buffer
}

/// Fetch the global log buffer, installing the logger on first call.
pub fn global() -> LogBuffer {
    if let Some(existing) = GLOBAL.get() {
        existing.clone()
    } else {
        install()
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
