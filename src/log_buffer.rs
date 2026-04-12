//! A ring-buffer `log::Log` implementation so the GUI can render the recent
//! log stream in its Log tab. It also forwards every record to an inner
//! `env_logger` so the terminal output is unchanged.
//!
//! The UI reads new entries incrementally via `drain_since(seq)` — each
//! frame only clones the lines that arrived since the last read, which is
//! typically 0–2 lines at 60 fps. The old `snapshot()` approach cloned all
//! 1000 lines every frame regardless.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

/// How many records to keep in memory. ~2 KB per line × 1000 ≈ 2 MB worst
/// case, which is fine for a long-running desktop session.
const CAPACITY: usize = 1000;

#[derive(Clone, Debug)]
pub struct LogLine {
    /// Monotonic sequence number, assigned on push. The UI uses this to
    /// request only lines it hasn't seen yet.
    pub seq: u64,
    /// Unix millis when the record was captured.
    pub ts_ms: u64,
    pub level: Level,
    pub message: String,
}

/// In-memory ring buffer. Cloneable (`Arc` internally) so both the UI and
/// the logger can hold references.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    buf: VecDeque<LogLine>,
    /// Next sequence number to assign. Strictly increasing, never resets.
    next_seq: u64,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                buf: VecDeque::with_capacity(CAPACITY),
                next_seq: 1,
            })),
        }
    }

    pub fn push(&self, ts_ms: u64, level: Level, message: String) {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.buf.len() == CAPACITY {
            inner.buf.pop_front();
        }
        inner.buf.push_back(LogLine {
            seq,
            ts_ms,
            level,
            message,
        });
    }

    /// Return only the lines with `seq > after_seq`. The UI calls this
    /// once per frame with its last-seen sequence number. Typical cost:
    /// 0–2 clones per frame instead of 1000.
    ///
    /// Also returns the sequence number of the oldest entry still in the
    /// buffer (`oldest_seq`). If `after_seq < oldest_seq`, some entries
    /// were evicted since the last read — the caller should rebuild its
    /// cache from the returned lines (they represent the full tail of
    /// the buffer that's still available).
    pub fn drain_since(&self, after_seq: u64) -> DrainResult {
        let inner = self.inner.lock().unwrap();
        if inner.buf.is_empty() {
            return DrainResult {
                lines: Vec::new(),
                oldest_seq: 0,
                newest_seq: 0,
            };
        }
        let oldest_seq = inner.buf.front().unwrap().seq;
        let newest_seq = inner.buf.back().unwrap().seq;

        // If caller is behind the oldest entry, return everything so they
        // can rebuild their local cache.
        let effective_after = if after_seq < oldest_seq {
            0
        } else {
            after_seq
        };

        let lines: Vec<LogLine> = inner
            .buf
            .iter()
            .filter(|l| l.seq > effective_after)
            .cloned()
            .collect();

        DrainResult {
            lines,
            oldest_seq,
            newest_seq,
        }
    }
}

pub struct DrainResult {
    pub lines: Vec<LogLine>,
    /// Seq of the oldest entry still in the buffer. If caller's last-seen
    /// seq is below this, entries were lost to ring eviction.
    pub oldest_seq: u64,
    /// Seq of the newest entry in the buffer.
    pub newest_seq: u64,
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
        self.buffer.push(
            now_ms(),
            record.level(),
            record.args().to_string(),
        );
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
