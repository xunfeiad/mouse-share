//! A `log::Log` implementation backed by `tokio::mpsc::channel(1000)`.
//!
//! Logger threads call `try_send()` (lock-free CAS) to push log lines into
//! the channel. The UI thread calls `drain()` to batch-receive all pending
//! messages into its own `Vec` — no shared Mutex, no ring buffer scanning,
//! no seq tracking. The channel's bounded capacity (1000) provides natural
//! back-pressure: if the UI falls behind, the oldest unsent messages are
//! dropped by `try_send` returning `Full`.
//!
//! This replaces the previous `Mutex<Ring>` + `drain_since(seq)` design.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::sync::OnceLock;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct LogLine {
    /// Unix millis when the record was captured.
    pub ts_ms: u64,
    pub level: Level,
    pub message: String,
}

/// The sender half, held by `TeeLogger`. Cloneable so the global logger
/// (which must be `Send + Sync + 'static`) can hand out copies.
#[derive(Clone)]
struct LogSender {
    tx: mpsc::Sender<LogLine>,
}

/// The receiver half, held by the UI. NOT Clone — exactly one consumer.
pub struct LogReceiver {
    rx: mpsc::Receiver<LogLine>,
}

impl LogReceiver {
    /// Drain all pending log lines from the channel into `out`.
    /// Non-blocking: returns immediately when the channel is empty.
    /// Typical per-frame cost: 0–2 moves, zero allocation (appends to
    /// caller's existing Vec).
    pub fn drain(&mut self, out: &mut Vec<LogLine>) {
        while let Ok(line) = self.rx.try_recv() {
            out.push(line);
        }
    }
}

/// Create the channel pair. Called once at startup.
fn create_channel() -> (LogSender, LogReceiver) {
    let (tx, rx) = mpsc::channel(1000);
    (LogSender { tx }, LogReceiver { rx })
}

/// A `log::Log` implementation that writes to both `env_logger` (for
/// stderr) and a `tokio::mpsc` channel (for the GUI).
struct TeeLogger {
    inner: env_logger::Logger,
    sender: LogSender,
}

// Safety: env_logger::Logger is Send+Sync, mpsc::Sender is Send+Sync.
unsafe impl Sync for TeeLogger {}

impl Log for TeeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        self.inner.log(record);
        // try_send is non-blocking. If the channel is full (UI not
        // draining fast enough), we silently drop the line — this is
        // the back-pressure policy. No lock, no allocation beyond the
        // String that record.args().to_string() already requires.
        let _ = self.sender.tx.try_send(LogLine {
            ts_ms: now_ms(),
            level: record.level(),
            message: record.args().to_string(),
        });
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Global singleton for the receiver. The UI takes it once via `take_receiver()`.
static RECEIVER: OnceLock<std::sync::Mutex<Option<LogReceiver>>> = OnceLock::new();

/// Install the tee logger as the global `log` backend. Safe to call more
/// than once — subsequent calls are no-ops.
pub fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let (sender, receiver) = create_channel();

        // Stash the receiver for the UI to claim later.
        let _ = RECEIVER.set(std::sync::Mutex::new(Some(receiver)));

        let inner = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("info"),
        )
        .format_timestamp_millis()
        .build();
        let filter = inner.filter();

        let tee = TeeLogger { inner, sender };

        if log::set_boxed_logger(Box::new(tee)).is_ok() {
            log::set_max_level(filter);
        } else {
            log::set_max_level(LevelFilter::Info);
        }
    });
}

/// Take the log receiver. Returns `Some` on the first call, `None` after
/// that (there is exactly one consumer). The UI calls this at startup.
pub fn take_receiver() -> Option<LogReceiver> {
    install(); // ensure logger is installed
    RECEIVER
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|mut guard| guard.take())
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
