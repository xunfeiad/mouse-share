//! Clipboard sync on its own QUIC stream. The OS clipboard is accessed only
//! on the worker thread and only while its stream generation is enabled.
use anyhow::{ensure, Context, Result};
use quinn::{Connection, RecvStream, SendStream};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{mpsc, watch},
    task::JoinHandle,
    time,
};

const MAX_TEXT: usize = 64 * 1024 * 1024;
const CHUNK: usize = 32 * 1024;
const RETRY: Duration = Duration::from_secs(2);
const NO_INITIAL_TEXT: u32 = u32::MAX;

trait ClipboardBackend {
    fn read_text(&mut self) -> Result<String>;
    fn write_text(&mut self, text: &str) -> Result<()>;
}

impl ClipboardBackend for arboard::Clipboard {
    fn read_text(&mut self) -> Result<String> {
        Ok(self.get_text()?)
    }
    fn write_text(&mut self, text: &str) -> Result<()> {
        Ok(self.set_text(text)?)
    }
}

type OpenClipboard = fn() -> Result<Box<dyn ClipboardBackend>>;

fn open_clipboard() -> Result<Box<dyn ClipboardBackend>> {
    Ok(Box::new(arboard::Clipboard::new()?))
}

#[derive(Default)]
struct Gate {
    enabled: bool,
    generation: u64,
}

pub struct Sync {
    gate: Arc<Mutex<Gate>>,
    changes: watch::Sender<u64>,
    task: JoinHandle<()>,
}

impl Sync {
    pub fn new(connection: Connection, server: bool) -> Self {
        Self::with_backend(connection, server, open_clipboard)
    }

    fn with_backend(connection: Connection, server: bool, open: OpenClipboard) -> Self {
        let gate = Arc::new(Mutex::new(Gate::default()));
        let (changes, rx) = watch::channel(0);
        let task = tokio::spawn(supervise(connection, server, gate.clone(), rx, open));
        Self {
            gate,
            changes,
            task,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        let mut gate = self.gate.lock().unwrap();
        if gate.enabled != enabled {
            gate.enabled = enabled;
            gate.generation += 1;
            self.changes.send_replace(gate.generation);
        }
    }
}

impl Drop for Sync {
    fn drop(&mut self) {
        self.set_enabled(false);
        self.task.abort();
    }
}

struct Streams(SendStream, RecvStream);

impl Drop for Streams {
    fn drop(&mut self) {
        let _ = self.0.reset(0u8.into());
        let _ = self.1.stop(0u8.into());
    }
}

async fn supervise(
    connection: Connection,
    server: bool,
    gate: Arc<Mutex<Gate>>,
    mut changes: watch::Receiver<u64>,
    open: OpenClipboard,
) {
    loop {
        let generation = *changes.borrow_and_update();
        if !gate.lock().unwrap().enabled {
            if changes.changed().await.is_err() {
                return;
            }
            continue;
        }
        let result = tokio::select! {
            _ = connection.closed() => return,
            change = changes.changed() => {
                if change.is_err() { return; }
                continue;
            }
            result = synchronize(&connection, server, gate.clone(), generation, open) => result,
        };
        if let Err(e) = result {
            log::debug!("Clipboard stream ended: {e}");
        }
        tokio::select! {
            _ = connection.closed() => return,
            _ = changes.changed() => {}
            _ = time::sleep(RETRY) => {}
        }
    }
}

async fn synchronize(
    connection: &Connection,
    server: bool,
    gate: Arc<Mutex<Gate>>,
    generation: u64,
    open: OpenClipboard,
) -> Result<()> {
    let streams = if server {
        connection.open_bi().await?
    } else {
        connection.accept_bi().await?
    };
    let mut streams = Streams(streams.0, streams.1);
    streams.0.set_priority(-1)?;
    if server {
        streams.0.write_all(&[b'C']).await?;
    } else {
        ensure!(
            streams.1.read_u8().await? == b'C',
            "Invalid clipboard stream"
        );
    }

    let (to_os, mut from_network) = mpsc::channel(1);
    let (to_network, mut from_os) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            let result = open().and_then(|clipboard| {
                clipboard_worker(
                    clipboard,
                    gate,
                    generation,
                    server,
                    &mut from_network,
                    to_network,
                )
            });
            if let Err(e) = result {
                log::debug!("Clipboard worker stopped: {e}");
            }
        })?;

    tokio::select! {
        result = send_text(&mut streams.0, &mut from_os) => result,
        result = receive_text(&mut streams.1, to_os) => result,
    }
}

async fn send_text(
    stream: &mut SendStream,
    updates: &mut mpsc::Receiver<Option<String>>,
) -> Result<()> {
    while let Some(update) = updates.recv().await {
        let Some(text) = update else {
            stream.write_all(&NO_INITIAL_TEXT.to_be_bytes()).await?;
            continue;
        };
        stream.write_all(&(text.len() as u32).to_be_bytes()).await?;
        for chunk in text.as_bytes().chunks(CHUNK) {
            stream.write_all(chunk).await?;
        }
    }
    Ok(())
}

async fn receive_text(
    stream: &mut RecvStream,
    updates: mpsc::Sender<Option<String>>,
) -> Result<()> {
    loop {
        let size = stream.read_u32().await?;
        if size == NO_INITIAL_TEXT {
            updates
                .send(None)
                .await
                .context("Clipboard worker stopped")?;
            continue;
        }
        let size = size as usize;
        ensure!(size <= MAX_TEXT, "Clipboard exceeds text limit");
        let mut bytes = vec![0; size];
        for chunk in bytes.chunks_mut(CHUNK) {
            stream.read_exact(chunk).await?;
        }
        updates
            .send(Some(String::from_utf8(bytes)?))
            .await
            .context("Clipboard worker stopped")?;
    }
}

fn clipboard_worker(
    mut clipboard: Box<dyn ClipboardBackend>,
    gate: Arc<Mutex<Gate>>,
    generation: u64,
    server: bool,
    updates: &mut mpsc::Receiver<Option<String>>,
    out: mpsc::Sender<Option<String>>,
) -> Result<()> {
    let mut last = None;
    let mut received_initial = server;
    if server {
        let gate = gate.lock().unwrap();
        if !gate.enabled || gate.generation != generation {
            return Ok(());
        }
        last = clipboard.read_text().ok().filter(|text| can_send(text));
        out.try_send(last.clone())?;
    }
    let mut poll = std::time::Instant::now();
    loop {
        {
            let gate = gate.lock().unwrap();
            if !gate.enabled || gate.generation != generation || out.is_closed() {
                return Ok(());
            }
            match updates.try_recv() {
                Ok(update) => {
                    received_initial = true;
                    if let Some(text) = update.filter(|text| can_send(text)) {
                        clipboard.write_text(text.as_str())?;
                        last = Some(text);
                    }
                }
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            if received_initial && poll.elapsed() >= Duration::from_millis(250) {
                poll = std::time::Instant::now();
                if let Ok(text) = clipboard.read_text() {
                    if can_send(&text)
                        && last.as_ref() != Some(&text)
                        && out.try_send(Some(text.clone())).is_ok()
                    {
                        last = Some(text);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn can_send(text: &str) -> bool {
    text.len() <= MAX_TEXT && !text.trim_start().starts_with("MS2-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{devices::TLS_NAME, transport};

    struct MemoryClipboard(&'static Mutex<String>);

    impl ClipboardBackend for MemoryClipboard {
        fn read_text(&mut self) -> Result<String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn write_text(&mut self, text: &str) -> Result<()> {
            *self.0.lock().unwrap() = text.into();
            Ok(())
        }
    }

    static LEFT: Mutex<String> = Mutex::new(String::new());
    static RIGHT: Mutex<String> = Mutex::new(String::new());

    fn left() -> Result<Box<dyn ClipboardBackend>> {
        Ok(Box::new(MemoryClipboard(&LEFT)))
    }
    fn right() -> Result<Box<dyn ClipboardBackend>> {
        Ok(Box::new(MemoryClipboard(&RIGHT)))
    }

    async fn wait_until(ready: impl Fn() -> bool) -> Result<()> {
        time::timeout(Duration::from_secs(5), async {
            while !ready() {
                time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn clipboard_stream_stays_independent_and_reopens_after_cancel() -> Result<()> {
        let cert = rcgen::generate_simple_self_signed(vec![TLS_NAME.into()])?;
        let der = cert.cert.der().to_vec();
        let listener = transport::server(
            "127.0.0.1:0".parse()?,
            der.clone(),
            cert.signing_key.serialize_der(),
        )?;
        let address = listener.local_addr()?.to_string();
        let (client, server) = tokio::join!(transport::connect(&address, der), async {
            listener.accept().await.unwrap().await
        },);
        let (_endpoint, client) = client?;
        let server = server?;
        let (mut tx, mut rx) = client.open_bi().await?;
        tx.write_all(&[1]).await?;
        let (mut reply, mut request) = server.accept_bi().await?;
        assert_eq!(request.read_u8().await?, 1);
        let mut sender = Sync::with_backend(server, true, left);
        let mut receiver = Sync::with_backend(client, false, right);
        // Keep both starting values equal so that the next update has a
        // single writer. The OS pasteboard is never used.
        sender.set_enabled(true);
        receiver.set_enabled(true);
        time::sleep(Duration::from_millis(400)).await;
        let size = 8 * 1024 * 1024;
        *LEFT.lock().unwrap() = "a".repeat(size);
        reply.write_all(&[2]).await?;
        assert_eq!(
            time::timeout(Duration::from_secs(1), rx.read_u8()).await??,
            2
        );
        wait_until(|| RIGHT.lock().unwrap().len() == size).await?;

        receiver.set_enabled(false);
        *LEFT.lock().unwrap() = "update while receiver disabled".into();
        time::sleep(Duration::from_millis(500)).await;
        assert_eq!(RIGHT.lock().unwrap().len(), size);

        // One side's STOP_SENDING/RESET ends both directions of the old stream.
        receiver.set_enabled(true);
        wait_until(|| RIGHT.lock().unwrap().as_str() == "update while receiver disabled").await?;
        Ok(())
    }

    #[test]
    fn disabled_generation_cannot_apply_a_buffered_update() -> Result<()> {
        static TEST: Mutex<String> = Mutex::new(String::new());
        let gate = Arc::new(Mutex::new(Gate {
            enabled: true,
            generation: 2,
        }));
        let (tx, mut updates) = mpsc::channel(1);
        let (out, _rx) = mpsc::channel(1);
        tx.try_send(Some("stale data".into()))?;
        clipboard_worker(
            Box::new(MemoryClipboard(&TEST)),
            gate,
            1,
            false,
            &mut updates,
            out,
        )?;
        assert!(TEST.lock().unwrap().is_empty());
        Ok(())
    }
}
