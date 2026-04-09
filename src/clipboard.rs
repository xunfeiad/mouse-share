use anyhow::Result;
use arboard::Clipboard;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;
const RETRY_DELAY: Duration = Duration::from_secs(2);

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

fn read_framed(stream: &mut TcpStream) -> Result<ClipboardMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!("clipboard message too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(bincode::deserialize(&buf)?)
}

/// Poll local clipboard and send changes over TCP.
fn watch_and_send(mut stream: TcpStream, state: Arc<ClipboardState>) -> Result<()> {
    let mut clipboard = Clipboard::new()
        .map_err(|e| anyhow::anyhow!("failed to open clipboard: {}", e))?;

    loop {
        std::thread::sleep(POLL_INTERVAL);

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
fn recv_and_apply(mut stream: TcpStream, state: Arc<ClipboardState>) -> Result<()> {
    let mut clipboard = Clipboard::new()
        .map_err(|e| anyhow::anyhow!("failed to open clipboard: {}", e))?;

    loop {
        let msg = read_framed(&mut stream)?;
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
fn handle_connection(stream: TcpStream) {
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

    let send_handle = std::thread::Builder::new()
        .name("clipboard-send".into())
        .spawn(move || {
            if let Err(e) = watch_and_send(send_stream, send_state) {
                log::info!("clipboard send thread exited: {}", e);
            }
        });

    let recv_handle = std::thread::Builder::new()
        .name("clipboard-recv".into())
        .spawn(move || {
            if let Err(e) = recv_and_apply(recv_stream, recv_state) {
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
/// Runs forever, reconnecting if a client disconnects.
pub fn run_server(port: u16) {
    let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)) {
        Ok(l) => l,
        Err(e) => {
            log::error!("failed to bind clipboard TCP port {}: {}", port, e);
            return;
        }
    };
    log::info!("Clipboard TCP listening on 0.0.0.0:{}", port);

    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                log::info!("Clipboard client connected: {}", addr);
                if let Err(e) = stream.set_nodelay(true) {
                    log::warn!("failed to set TCP_NODELAY: {}", e);
                }
                handle_connection(stream);
                log::info!("Clipboard client disconnected");
            }
            Err(e) => {
                log::error!("clipboard accept error: {}", e);
                std::thread::sleep(RETRY_DELAY);
            }
        }
    }
}

/// Client-side clipboard: connect to server and handle the connection.
/// Retries forever on disconnect.
pub fn run_client(addr: SocketAddr) {
    loop {
        log::info!("Connecting to clipboard server at {}...", addr);
        match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(stream) => {
                log::info!("Clipboard TCP connected to {}", addr);
                if let Err(e) = stream.set_nodelay(true) {
                    log::warn!("failed to set TCP_NODELAY: {}", e);
                }
                handle_connection(stream);
                log::info!("Clipboard TCP disconnected, will retry");
            }
            Err(e) => {
                log::warn!("clipboard connect failed: {}", e);
            }
        }
        std::thread::sleep(RETRY_DELAY);
    }
}
