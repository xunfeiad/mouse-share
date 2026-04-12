//! A fixed-size ring-buffer `log::Log` implementation so the GUI can render
//! the recent log stream in its Log tab. It also forwards every record to an
//! inner `env_logger` so the terminal output is unchanged.
//!
//! The buffer is pre-allocated once at startup and never reallocates. Writes
//! overwrite the oldest slot in O(1) constant time — no `pop_front`, no
//! `realloc`, no jitter. The UI reads new entries incrementally via
//! `drain_since(seq)`, cloning only the lines that arrived since its last
//! read (typically 0–2 per frame).

use log::{Level, LevelFilter, Log, Metadata, Record};
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

/// Fixed-size ring buffer. Pre-allocates `CAPACITY` slots once; subsequent
/// writes overwrite the oldest slot via a wrapping write cursor — zero
/// reallocation, O(1) constant push, no jitter.
///
/// Cloneable (`Arc` internally) so both the UI and the logger can hold
/// references.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Ring>>,
}

/// The actual ring storage. `slots` is allocated once and never resized.
struct Ring {
    /// Pre-allocated storage. Slots `0..len` contain valid entries;
    /// `write_idx` is where the next entry will be written.
    slots: Box<[Option<LogLine>]>,
    /// Next position to write (wraps around via `% CAPACITY`).
    write_idx: usize,
    /// Number of valid entries, `0..=CAPACITY`.
    len: usize,
    /// Next sequence number to assign. Strictly increasing, never resets.
    next_seq: u64,
}

impl Ring {
    fn new() -> Self {
        // Pre-allocate all slots up front. After this, the buffer never
        // touches the allocator again (aside from the String inside each
        // LogLine, which is unavoidable).
        let slots: Vec<Option<LogLine>> = (0..CAPACITY).map(|_| None).collect();
        Self {
            slots: slots.into_boxed_slice(),
            write_idx: 0,
            len: 0,
            next_seq: 1,
        }
    }

    fn push(&mut self, ts_ms: u64, level: Level, message: String) {
        let seq = self.next_seq;
        self.next_seq += 1;

        // Overwrite the slot at write_idx. If the slot already held a
        // LogLine, its String is dropped here — no shifting, no memcpy.
        self.slots[self.write_idx] = Some(LogLine {
            seq,
            ts_ms,
            level,
            message,
        });
        self.write_idx = (self.write_idx + 1) % CAPACITY;
        if self.len < CAPACITY {
            self.len += 1;
        }
    }

    /// Index of the oldest valid entry.
    fn oldest_idx(&self) -> usize {
        if self.len < CAPACITY {
            0
        } else {
            self.write_idx // write_idx points at the oldest when full
        }
    }

    /// Iterate entries from oldest to newest.
    fn iter(&self) -> RingIter<'_> {
        RingIter {
            ring: self,
            pos: 0,
            remaining: self.len,
        }
    }
}

struct RingIter<'a> {
    ring: &'a Ring,
    pos: usize,   // how many items we've yielded
    remaining: usize,
}

impl<'a> Iterator for RingIter<'a> {
    type Item = &'a LogLine;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let idx = (self.ring.oldest_idx() + self.pos) % CAPACITY;
        self.pos += 1;
        self.remaining -= 1;
        self.ring.slots[idx].as_ref()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Ring::new())),
        }
    }

    pub fn push(&self, ts_ms: u64, level: Level, message: String) {
        self.inner.lock().unwrap().push(ts_ms, level, message);
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
        let ring = self.inner.lock().unwrap();
        if ring.len == 0 {
            return DrainResult {
                lines: Vec::new(),
                oldest_seq: 0,
                newest_seq: 0,
            };
        }

        let mut iter = ring.iter();
        let oldest_seq = iter.next().map(|l| l.seq).unwrap_or(0);
        // We consumed one item from iter; we need to check it too,
        // so collect from the ring's iter directly instead.
        drop(iter);

        let newest_seq = ring.next_seq - 1;

        // Fast path: caller is up to date.
        if after_seq >= newest_seq {
            return DrainResult {
                lines: Vec::new(),
                oldest_seq,
                newest_seq,
            };
        }

        // If caller is behind the oldest entry, return everything so
        // they can rebuild their local cache.
        let effective_after = if after_seq < oldest_seq {
            0
        } else {
            after_seq
        };

        let lines: Vec<LogLine> = ring
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
