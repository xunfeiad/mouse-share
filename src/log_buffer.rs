//! A `log::Log` implementation backed by `tokio::mpsc::channel(1000)`.
//!
//! Logger threads call `try_send()` (lock-free CAS) to push log lines into
//! the channel. The UI thread calls `drain()` to batch-receive all pending
//! messages into its own `Vec` — no shared Mutex, no ring buffer scanning,
//! no seq tracking. The channel's bounded capacity (1000) provides natural
//! back-pressure: if the UI falls behind, new messages are
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
    stderr: crossbeam_channel::Sender<LogLine>,
    sender: LogSender,
}

// Safety: env_logger::Logger is Send+Sync, mpsc::Sender is Send+Sync.
unsafe impl Sync for TeeLogger {}

impl Log for TeeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Formatting/output belongs to the consumer; capture callbacks never
        // synchronously write stderr.
        // try_send is non-blocking. If the channel is full (UI not
        // draining fast enough), we silently drop the line — this is
        // the back-pressure policy. No lock, no allocation beyond the
        // String that record.args().to_string() already requires.
        let line = LogLine {
            ts_ms: now_ms(),
            level: record.level(),
            message: record.args().to_string(),
        };
        let _ = self.stderr.try_send(line.clone());
        let _ = self.sender.tx.try_send(line);
    }

    fn flush(&self) {}
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

        let inner =
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("trace"))
                .format_timestamp_millis()
                .build();
        let filter = LevelFilter::Info;

        let (stderr, output) = crossbeam_channel::bounded::<LogLine>(1000);
        let _ = std::thread::Builder::new()
            .name("log-output".into())
            .spawn(move || {
                for line in output {
                    inner.log(
                        &Record::builder()
                            .level(line.level)
                            .target("mouse_share")
                            .args(format_args!("{}", line.message))
                            .build(),
                    );
                }
            });
        let tee = TeeLogger { stderr, sender };

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
