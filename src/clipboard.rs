use anyhow::Result;
use arboard::Clipboard;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const IO_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Helper: sleep in small slices so a shutdown request isn't delayed by a
/// long sleep. Returns `true` if shutdown was requested during the wait.
fn interruptible_sleep(total: Duration, shutdown: &Arc<AtomicBool>) -> bool {
    let slice = Duration::from_millis(50);
    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        if shutdown.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(slice);
        elapsed += slice;
    }
    shutdown.load(Ordering::SeqCst)
}

#[derive(Serialize, Deserialize, Clone, Debug, Hash)]
pub enum ClipboardMessage {
    Text(String),
}

/// Shared state between send and recv threads to prevent sync loops.
/// The mutex serializes clipboard read/write operations so the watcher
/// never sees a stale clipboard with an up-to-date hash (or vice versa).
pub struct ClipboardState {
    last_hash: Mutex<Option<u64>>,
}

impl ClipboardState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            last_hash: Mutex::new(None),
        })
    }
}

fn hash_of(msg: &ClipboardMessage) -> u64 {
    let mut hasher = DefaultHasher::new();
    msg.hash(&mut hasher);
    hasher.finish()
}

fn msg_summary(msg: &ClipboardMessage) -> String {
    match msg {
        ClipboardMessage::Text(s) => {
            let preview: String = s.chars().take(20).collect();
            format!("Text({} chars: {:?})", s.len(), preview)
        }
    }
}

/// Length-prefixed framing over TCP.
/// Format: [4-byte BE length][bincode payload]
fn write_framed(stream: &mut TcpStream, msg: &ClipboardMessage) -> Result<()> {
    let data = bincode::serialize(msg)?;
    let len = (data.len() as u32).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(&data)?;
    stream.flush()?;
    Ok(())
}

/// Attempt to read one framed message. Returns `Ok(None)` if the read hits
/// the socket read timeout (no data yet), `Ok(Some(msg))` on a complete
/// message, and `Err(_)` on disconnect or protocol error. Used by the recv
/// loop so it can periodically check the shutdown flag.
fn read_framed_or_timeout(
    stream: &mut TcpStream,
    shutdown: &Arc<AtomicBool>,
) -> Result<Option<ClipboardMessage>> {
    let mut len_buf = [0u8; 4];
    // Poll the length prefix byte-by-byte so a mid-message shutdown still
    // exits within one IO_TIMEOUT slice.
    let mut read = 0;
    while read < 4 {
        match stream.read(&mut len_buf[read..]) {
            Ok(0) => anyhow::bail!("clipboard peer closed connection"),
            Ok(n) => read += n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if shutdown.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                // Keep waiting for the rest of the length prefix.
            }
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!("clipboard message too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    let mut read = 0;
    while read < len {
        match stream.read(&mut buf[read..]) {
            Ok(0) => anyhow::bail!("clipboard peer closed connection"),
            Ok(n) => read += n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if shutdown.load(Ordering::SeqCst) {
                    return Ok(None);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(bincode::deserialize(&buf)?))
}

/// Poll local clipboard and send changes over TCP.
fn watch_and_send(
    mut stream: TcpStream,
    state: Arc<ClipboardState>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let mut clipboard = Clipboard::new()
        .map_err(|e| anyhow::anyhow!("failed to open clipboard: {}", e))?;

    loop {
        if interruptible_sleep(POLL_INTERVAL, &shutdown) {
            return Ok(());
        }

        // Atomic: read clipboard + check hash + update hash, all under lock.
        // This prevents the race where recv updates hash+clipboard between
        // the watcher reading clipboard and checking the hash.
        let to_send = {
            let mut guard = state.last_hash.lock().unwrap();
            match clipboard.get_text() {
                Ok(text) if !text.is_empty() => {
                    let msg = ClipboardMessage::Text(text);
                    let hash = hash_of(&msg);
                    if *guard != Some(hash) {
                        *guard = Some(hash);
                        Some(msg)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };

        if let Some(msg) = to_send {
            log::info!("Clipboard -> peer: {}", msg_summary(&msg));
            write_framed(&mut stream, &msg)?;
        }
    }
}

/// Receive clipboard updates from TCP and apply to local clipboard.
fn recv_and_apply(
    mut stream: TcpStream,
    state: Arc<ClipboardState>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let mut clipboard = Clipboard::new()
        .map_err(|e| anyhow::anyhow!("failed to open clipboard: {}", e))?;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        let msg = match read_framed_or_timeout(&mut stream, &shutdown)? {
            Some(m) => m,
            None => return Ok(()), // shutdown requested
        };
        let hash = hash_of(&msg);

        // Atomic: update hash + apply to clipboard under lock.
        // The watcher, if polling concurrently, will block here and
        // then see the new clipboard content matching the new hash.
        {
            let mut guard = state.last_hash.lock().unwrap();
            *guard = Some(hash);
            match &msg {
                ClipboardMessage::Text(text) => {
                    if let Err(e) = clipboard.set_text(text.clone()) {
                        log::error!("failed to set clipboard: {}", e);
                    }
                }
            }
        }

        log::info!("Clipboard <- peer: {}", msg_summary(&msg));
    }
}

/// Handle a single TCP connection for bidirectional clipboard sync.
/// Spawns two worker threads and waits for either to fail.
fn handle_connection(stream: TcpStream, shutdown: Arc<AtomicBool>) {
    // Timeouts let both read and write return periodically so the threads
    // can notice `shutdown` instead of blocking indefinitely.
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let send_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::error!("failed to clone TCP stream: {}", e);
            return;
        }
    };
    let recv_stream = stream;

    let state = ClipboardState::new();
    let send_state = state.clone();
    let recv_state = state;
    let send_shutdown = shutdown.clone();
    let recv_shutdown = shutdown;

    let send_handle = std::thread::Builder::new()
        .name("clipboard-send".into())
        .spawn(move || {
            if let Err(e) = watch_and_send(send_stream, send_state, send_shutdown) {
                log::info!("clipboard send thread exited: {}", e);
            }
        });

    let recv_handle = std::thread::Builder::new()
        .name("clipboard-recv".into())
        .spawn(move || {
            if let Err(e) = recv_and_apply(recv_stream, recv_state, recv_shutdown) {
                log::info!("clipboard recv thread exited: {}", e);
            }
        });

    if let Ok(h) = send_handle {
        let _ = h.join();
    }
    if let Ok(h) = recv_handle {
        let _ = h.join();
    }
}

/// Server-side clipboard: accept TCP connections and handle them.
/// Loops until `shutdown` is set.
pub fn run_server(port: u16, shutdown: Arc<AtomicBool>) {
    let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)) {
        Ok(l) => l,
        Err(e) => {
            log::error!("failed to bind clipboard TCP port {}: {}", port, e);
            return;
        }
    };
    if let Err(e) = listener.set_nonblocking(true) {
        log::error!("failed to set clipboard listener nonblocking: {}", e);
        return;
    }
    log::info!("Clipboard TCP listening on 0.0.0.0:{}", port);

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, addr)) => {
                log::info!("Clipboard client connected: {}", addr);
                if let Err(e) = stream.set_nodelay(true) {
                    log::warn!("failed to set TCP_NODELAY: {}", e);
                }
                handle_connection(stream, shutdown.clone());
                log::info!("Clipboard client disconnected");
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                log::error!("clipboard accept error: {}", e);
                if interruptible_sleep(RETRY_DELAY, &shutdown) {
                    return;
                }
            }
        }
    }
}

/// Client-side clipboard: connect to server and handle the connection.
/// Retries on disconnect until `shutdown` is set.
pub fn run_client(addr: SocketAddr, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::SeqCst) {
        log::info!("Connecting to clipboard server at {}...", addr);
        match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(stream) => {
                log::info!("Clipboard TCP connected to {}", addr);
                if let Err(e) = stream.set_nodelay(true) {
                    log::warn!("failed to set TCP_NODELAY: {}", e);
                }
                handle_connection(stream, shutdown.clone());
                log::info!("Clipboard TCP disconnected, will retry");
            }
            Err(e) => {
                log::warn!("clipboard connect failed: {}", e);
            }
        }
        if interruptible_sleep(RETRY_DELAY, &shutdown) {
            return;
        }
    }
}
