//! A cancellable, single-worker, full-duplex clipboard connection. Each
//! connection is bound to one input session and one enable/disable epoch.
use crate::{
    net::{
        state::SharedState,
        transport::{self, Framed},
    },
    protocol::SessionId,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    net::{SocketAddr, TcpListener},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const LIMIT: usize = 64 * 1024 * 1024;
pub trait ClipboardBackend: Send {
    fn get(&mut self) -> Result<Option<String>>;
    fn set(&mut self, value: String) -> Result<()>;
}
pub struct NativeClipboard(arboard::Clipboard);
impl NativeClipboard {
    pub fn open() -> Result<Box<dyn ClipboardBackend>> {
        Ok(Box::new(Self(arboard::Clipboard::new()?)))
    }
}
impl ClipboardBackend for NativeClipboard {
    fn get(&mut self) -> Result<Option<String>> {
        Ok(self.0.get_text().ok())
    }
    fn set(&mut self, value: String) -> Result<()> {
        self.0.set_text(value)?;
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
enum Packet {
    Bind {
        magic: [u8; 4],
        session: SessionId,
        epoch: u64,
    },
    Ready,
    Text(String),
}

pub struct ClipboardWorker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl ClipboardWorker {
    pub fn start(
        addr: SocketAddr,
        server: bool,
        session: SessionId,
        epoch: u64,
        shared: Arc<SharedState>,
        factory: Arc<dyn Fn() -> Result<Box<dyn ClipboardBackend>> + Send + Sync>,
    ) -> Result<Self> {
        // Bind synchronously so a failed enable is observable before ack.
        let listener = if server {
            let l = TcpListener::bind(addr)?;
            l.set_nonblocking(true)?;
            Some(l)
        } else {
            None
        };
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        let thread = std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let mut clipboard = match factory() {
                    Ok(c) => c,
                    Err(e) => {
                        shared.clipboard_error(Some(format!("Clipboard unavailable: {e}")));
                        return;
                    }
                };
                while !cancelled.load(Ordering::SeqCst) {
                    let stream = if let Some(listener) = &listener {
                        match listener.accept() {
                            Ok((stream, _)) => Some(stream),
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                            Err(e) => {
                                shared.clipboard_error(Some(e.to_string()));
                                None
                            }
                        }
                    } else {
                        match transport::connect(addr, &cancelled, Duration::from_millis(500)) {
                            Ok(s) => Some(s),
                            Err(e) => {
                                if !cancelled.load(Ordering::SeqCst) {
                                    shared.clipboard_error(Some(format!(
                                        "Clipboard reconnecting: {e}"
                                    )));
                                }
                                None
                            }
                        }
                    };
                    if let Some(stream) = stream {
                        let result = Framed::new(stream, LIMIT).and_then(|wire| {
                            exchange(
                                wire,
                                &mut *clipboard,
                                server,
                                session,
                                epoch,
                                &cancelled,
                                &shared,
                            )
                        });
                        if let Err(e) = result {
                            if !cancelled.load(Ordering::SeqCst) {
                                shared
                                    .clipboard_error(Some(format!("Clipboard disconnected: {e}")));
                            }
                        }
                    }
                    let until =
                        Instant::now() + Duration::from_millis(if server { 20 } else { 200 });
                    while Instant::now() < until && !cancelled.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}
impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
fn exchange(
    mut wire: Framed,
    clipboard: &mut dyn ClipboardBackend,
    server: bool,
    session: SessionId,
    epoch: u64,
    stop: &AtomicBool,
    shared: &SharedState,
) -> Result<()> {
    if !server {
        wire.queue(&Packet::Bind {
            magic: crate::protocol::MAGIC,
            session,
            epoch,
        })?;
    }
    let start = Instant::now();
    let mut ready = false;
    let mut last_text = None;
    let mut poll = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        wire.flush()?;
        if let Some(packet) = wire.recv::<Packet>()? {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match packet {
                Packet::Bind {
                    magic,
                    session: id,
                    epoch: e,
                } if server
                    && !ready
                    && magic == crate::protocol::MAGIC
                    && id == session
                    && e == epoch =>
                {
                    wire.queue(&Packet::Ready)?;
                    ready = true;
                    shared.clipboard_error(None);
                }
                Packet::Ready if !server && !ready => {
                    ready = true;
                    shared.clipboard_error(None);
                }
                Packet::Text(text) if ready => {
                    // The cancellation flag is checked immediately before
                    // touching the OS clipboard, including partial frames.
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    clipboard.set(text.clone())?;
                    last_text = Some(text);
                }
                _ => anyhow::bail!("Clipboard session mismatch"),
            }
        }
        if !ready && start.elapsed() > Duration::from_secs(2) {
            anyhow::bail!("Clipboard handshake timeout");
        }
        if ready && poll.elapsed() >= Duration::from_millis(500) && !wire.pending() {
            poll = Instant::now();
            if let Some(text) = clipboard.get()? {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if last_text.as_ref() != Some(&text) {
                    anyhow::ensure!(text.len() < LIMIT - 64, "Clipboard text exceeds 64 MiB");
                    wire.queue(&Packet::Text(text.clone()))?;
                    last_text = Some(text);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
