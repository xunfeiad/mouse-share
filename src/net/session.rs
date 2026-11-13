use super::{
    devices::{local_name, random, DeviceId, Devices, Features, Invite, PairedDevice, Token},
    input::{Capture, Forward, RemoteInput},
    state::{now_ms, SharedState},
    transport::{self, HANDSHAKE},
};
use crate::{
    clipboard,
    config::Edge,
    input::capture::CapturedInput,
    protocol::{Control, Login, LoginReply, MouseEvent, ScreenInfo},
    screen,
};
use anyhow::{bail, ensure, Context, Result};
use quinn::{Connection, RecvStream, SendStream};
use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{mpsc, watch, Mutex},
    task::JoinHandle,
    time::{self, Instant},
};
use zeroize::Zeroizing;

const HEARTBEAT: Duration = Duration::from_secs(1);
const DEADLINE: Duration = Duration::from_secs(5);
const SNAPSHOT: Duration = Duration::from_millis(250);
const FRAME: Duration = Duration::from_millis(5);
const ENTER_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_PENDING_INPUT: usize = 512;

pub enum SessionRole {
    Server { port: u16, edge: Edge },
    Client { server: PairedDevice },
}

pub struct SessionConfig {
    pub devices: Devices,
    pub role: SessionRole,
}

pub enum SessionCommand {
    Stop,
    Features {
        keyboard: Option<bool>,
        clipboard: Option<bool>,
    },
    Invite(String),
    Revoke(DeviceId),
}

struct Invitation {
    token: Zeroizing<Token>,
    expires: Instant,
}

struct Authenticated {
    connection: Connection,
    tx: SendStream,
    rx: RecvStream,
    peer: DeviceId,
    screen: ScreenInfo,
    mac: bool,
}

pub fn run(
    config: SessionConfig,
    state: Arc<SharedState>,
    commands: mpsc::Receiver<SessionCommand>,
) -> Result<()> {
    state.started_ms.store(now_ms(), Ordering::Release);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        match config.role {
            SessionRole::Server { port, edge } => {
                serve(config.devices, port, edge, &state, commands, Capture::start).await
            }
            SessionRole::Client { server } => {
                connect(config.devices, server, &state, commands).await
            }
        }
    });
    runtime.shutdown_timeout(Duration::from_millis(200));
    state.connected.store(false, Ordering::Release);
    state.mouse_on_peer.store(false, Ordering::Release);
    state.invitation.lock().unwrap().take();
    result
}

pub async fn pair(devices: Devices, invite: Invite) -> Result<PairedDevice> {
    let server = invite.server;
    let token = Zeroizing::new(invite.token);
    let id = devices.id()?;
    let (_endpoint, connection) = transport::connect(
        server
            .address
            .as_deref()
            .context("Missing server address")?,
        server.certificate.clone(),
    )
    .await?;
    time::timeout(HANDSHAKE, async {
        let (mut tx, mut rx) = connection.open_bi().await?;
        transport::send(
            &mut tx,
            &Login::Pair {
                id,
                name: local_name(),
                invite: *token,
            },
        )
        .await?;
        let credential = match transport::receive(&mut rx).await? {
            LoginReply::Paired(token) => Zeroizing::new(token),
            LoginReply::Rejected(message) => bail!("{message}"),
            _ => bail!("Unexpected pairing reply"),
        };
        let store = devices.clone();
        let saved = server.clone();
        tokio::task::spawn_blocking(move || store.add(saved, &credential)).await??;
        let finish = async {
            if let Err(e) = tx.write_all(&[1]).await {
                devices.forget(server.id)?;
                return Err(e.into());
            }
            match transport::receive(&mut rx).await? {
                LoginReply::Saved => Ok(()),
                LoginReply::Rejected(message) => {
                    devices.forget(server.id)?;
                    bail!("{message}");
                }
                _ => bail!("Unexpected pairing confirmation"),
            }
        };
        // After sending the commit marker, retain the credential on an
        // ambiguous transport error. The server may have committed it.
        finish.await?;
        Ok(server)
    })
    .await
    .context("Pairing timed out")?
}

async fn authenticate(
    incoming: quinn::Incoming,
    devices: Devices,
    invite: Arc<Mutex<Option<Invitation>>>,
) -> Result<Option<Authenticated>> {
    let connection = incoming.await?;
    let (mut tx, mut rx) = connection.accept_bi().await?;
    match transport::receive::<Login>(&mut rx).await? {
        Login::Pair {
            id,
            name,
            invite: offered,
        } => {
            let offered = Zeroizing::new(offered);
            let mut lease = invite.lock().await;
            let valid = lease.as_ref().is_some_and(|entry| {
                Instant::now() < entry.expires
                    && bool::from(entry.token.as_slice().ct_eq(offered.as_slice()))
            });
            if !valid || id == devices.id()? || devices.list()?.iter().any(|device| device.id == id)
            {
                transport::send(
                    &mut tx,
                    &LoginReply::Rejected(
                        "Invitation invalid, expired or already used, or device already paired"
                            .into(),
                    ),
                )
                .await?;
                tx.finish()?;
                let _ = tx.stopped().await;
                return Ok(None);
            }
            let credential = Zeroizing::new(random()?);
            transport::send(&mut tx, &LoginReply::Paired(*credential)).await?;
            let mut confirmed = [0];
            rx.read_exact(&mut confirmed).await?;
            ensure!(confirmed == [1], "Pairing was not confirmed");
            if lease
                .as_ref()
                .is_none_or(|entry| Instant::now() >= entry.expires)
            {
                transport::send(
                    &mut tx,
                    &LoginReply::Rejected("Pairing invitation expired".into()),
                )
                .await?;
                tx.finish()?;
                let _ = tx.stopped().await;
                return Ok(None);
            }
            let device = PairedDevice {
                id,
                name: name.chars().filter(|c| !c.is_control()).take(64).collect(),
                address: None,
                certificate: Vec::new(),
            };
            // Consume the invitation before committing so a slow credential
            // store cannot make the invitation reusable after a timeout.
            lease.take();
            drop(lease);
            let result =
                tokio::task::spawn_blocking(move || devices.add(device, &credential)).await?;
            if let Err(e) = result {
                log::warn!("Could not save client credential: {e}");
                transport::send(
                    &mut tx,
                    &LoginReply::Rejected("Could not save client credential".into()),
                )
                .await?;
                tx.finish()?;
                let _ = tx.stopped().await;
                return Ok(None);
            }
            transport::send(&mut tx, &LoginReply::Saved).await?;
            tx.finish()?;
            let _ = tx.stopped().await;
            Ok(None)
        }
        Login::Connect {
            id,
            token,
            screen,
            mac,
        } => {
            let token = Zeroizing::new(token);
            let authorized = screen.valid()
                && tokio::task::spawn_blocking(move || authorized(&devices, id, &token)).await?;
            if !authorized {
                transport::send(
                    &mut tx,
                    &LoginReply::Rejected(
                        "Device not authorized or screen information invalid".into(),
                    ),
                )
                .await?;
                tx.finish()?;
                let _ = tx.stopped().await;
                return Ok(None);
            }
            Ok(Some(Authenticated {
                connection,
                tx,
                rx,
                peer: id,
                screen,
                mac,
            }))
        }
    }
}

fn authorized(devices: &Devices, id: DeviceId, token: &Token) -> bool {
    devices.list().is_ok_and(|list| {
        list.iter()
            .any(|device| device.id == id && device.address.is_none())
    }) && devices
        .token(id)
        .is_ok_and(|expected| bool::from(expected.as_slice().ct_eq(token)))
}

struct Active {
    id: DeviceId,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Active {
    async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

async fn serve(
    devices: Devices,
    port: u16,
    edge: Edge,
    state: &Arc<SharedState>,
    mut commands: mpsc::Receiver<SessionCommand>,
    start_capture: fn() -> Result<Capture>,
) -> Result<()> {
    let (id, cert, key) = devices.identity()?;
    let endpoint = transport::server(
        SocketAddr::from(([0, 0, 0, 0], port)),
        cert.clone(),
        key.to_vec(),
    )?;
    let port = endpoint.local_addr()?.port();
    let local_screen = screen::get_screen_info()?;
    ensure!(local_screen.valid(), "Invalid local screen dimensions");
    let invite = Arc::new(Mutex::new(None::<Invitation>));
    let initial = devices.features()?;
    *state.features.lock().unwrap() = initial;
    let (features, _) = watch::channel(initial);
    let mut pending = tokio::task::JoinSet::new();
    let mut active: Option<Active> = None;
    let mut tick = time::interval(Duration::from_millis(100));
    log::info!("Listening for paired devices on UDP {port}");
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(SessionCommand::Stop) | None => break,
                Some(SessionCommand::Features { keyboard, clipboard }) => {
                    let mut value = *features.borrow();
                    if let Some(keyboard) = keyboard { value.keyboard = keyboard; }
                    if let Some(clipboard) = clipboard { value.clipboard = clipboard; }
                    match devices.set_features(value) {
                        Ok(()) => {
                            features.send_replace(value);
                            if !state.connected.load(Ordering::Acquire) {
                                *state.features.lock().unwrap() = value;
                            }
                        }
                        Err(e) => state.set_error(e.to_string()),
                    }
                }
                Some(SessionCommand::Invite(host)) => {
                    let result = async {
                        let host = host.trim();
                        ensure!(!host.is_empty() && host.len() <= 253 &&
                            host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
                            "Enter the server's IPv4 address or hostname");
                        let token = Zeroizing::new(random()?);
                        let code = Invite {
                            server: PairedDevice { id, name: local_name(), address: Some(format!("{host}:{port}")), certificate: cert.clone() },
                            token: *token,
                        }.encode()?;
                        *invite.try_lock().map_err(|_| anyhow::anyhow!("A pairing request is in progress"))? =
                            Some(Invitation { token, expires: Instant::now() + Duration::from_secs(300) });
                        *state.invitation.lock().unwrap() = Some(code);
                        Ok::<_, anyhow::Error>(())
                    }.await;
                    if let Err(e) = result { state.set_error(e.to_string()); }
                }
                Some(SessionCommand::Revoke(peer)) => {
                    if active.as_ref().is_some_and(|a| a.id == peer) {
                        active.take().unwrap().stop().await;
                    }
                    let store = devices.clone();
                    if let Err(e) = tokio::task::spawn_blocking(move || store.forget(peer)).await? {
                        state.set_error(e.to_string());
                    }
                }
            },
            incoming = endpoint.accept() => {
                if let Some(incoming) = incoming {
                    if pending.len() < 8 {
                        pending.spawn(time::timeout(HANDSHAKE, authenticate(incoming, devices.clone(), invite.clone())));
                    } else { incoming.refuse(); }
                }
            }
            Some(result) = pending.join_next() => {
                if let Ok(Ok(Ok(Some(mut peer)))) = result {
                    if !devices.list().is_ok_and(|list| list.iter().any(|d| d.id == peer.peer && d.address.is_none())) {
                        peer.connection.close(1u8.into(), b"revoked");
                        continue;
                    }
                    if active.as_ref().is_some_and(|a| a.id != peer.peer && !a.task.is_finished()) {
                        let _ = time::timeout(HEARTBEAT, transport::send(&mut peer.tx, &LoginReply::Rejected("Server is already controlling another device".into()))).await;
                        let _ = peer.tx.finish();
                        pending.spawn(async move {
                            let _ = time::timeout(HEARTBEAT, peer.tx.stopped()).await;
                            Ok(Ok(None))
                        });
                        continue;
                    }
                    if let Some(previous) = active.take() { previous.stop().await; }
                    let (stop, cancelled) = watch::channel(false);
                    let state = state.clone();
                    let config = features.subscribe();
                    let peer_id = peer.peer;
                    let task = tokio::spawn(async move {
                        let result = serve_peer(peer, local_screen, edge, &state, config.clone(), cancelled, start_capture).await;
                        *state.features.lock().unwrap() = *config.borrow();
                        state.keyboard_supported.store(true, Ordering::Release);
                        state.connected.store(false, Ordering::Release);
                        state.mouse_on_peer.store(false, Ordering::Release);
                        if let Err(e) = result { state.set_error(e.to_string()); }
                    });
                    active = Some(Active { id: peer_id, stop, task });
                }
                if let Ok(mut lease) = invite.try_lock() {
                    if lease.as_ref().is_some_and(|entry| Instant::now() >= entry.expires) {
                        lease.take();
                    }
                    if lease.is_none() { state.invitation.lock().unwrap().take(); }
                }
            }
            _ = tick.tick() => {
                if state.shutdown.load(Ordering::Acquire) { break; }
                if active.as_ref().is_some_and(|peer| peer.task.is_finished()) {
                    active.take().unwrap().stop().await;
                    *state.features.lock().unwrap() = *features.borrow();
                    state.keyboard_supported.store(true, Ordering::Release);
                }
                if let Some(peer) = &active {
                    if !devices.list().is_ok_and(|list| list.iter().any(|d| d.id == peer.id && d.address.is_none())) {
                        active.take().unwrap().stop().await;
                    }
                }
                if let Ok(mut lease) = invite.try_lock() {
                    if lease.as_ref().is_some_and(|entry| Instant::now() >= entry.expires) {
                        lease.take();
                        state.invitation.lock().unwrap().take();
                    }
                }
            }
        }
    }
    if let Some(active) = active {
        active.stop().await;
    }
    pending.shutdown().await;
    endpoint.close(0u8.into(), b"server stopped");
    let _ = time::timeout(HEARTBEAT, endpoint.wait_idle()).await;
    Ok(())
}

struct Channel {
    connection: Connection,
    tx: SendStream,
    incoming: mpsc::Receiver<Result<Control>>,
    reader: JoinHandle<()>,
}

impl Channel {
    fn new(connection: Connection, tx: SendStream, mut rx: RecvStream) -> Self {
        let (send, incoming) = mpsc::channel(256);
        let reader = tokio::spawn(async move {
            loop {
                let message = transport::receive(&mut rx).await;
                let done = message.is_err();
                if send.send(message).await.is_err() || done {
                    break;
                }
            }
        });
        Self {
            connection,
            tx,
            incoming,
            reader,
        }
    }

    async fn send(&mut self, message: Control) -> Result<()> {
        time::timeout(HEARTBEAT, transport::send(&mut self.tx, &message))
            .await
            .context("Control stream stalled")?
    }

    async fn receive(&mut self) -> Result<Control> {
        self.incoming
            .recv()
            .await
            .context("Control stream closed")?
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.reader.abort();
        self.connection.close(0u8.into(), b"session ended");
    }
}

struct Liveness {
    pong: Instant,
    sent: u64,
    received: u64,
}

impl Liveness {
    fn new() -> Self {
        Self {
            pong: Instant::now(),
            sent: 0,
            received: 0,
        }
    }

    async fn ping(&mut self, link: &mut Channel) -> Result<()> {
        self.check()?;
        self.sent += 1;
        link.send(Control::Ping(self.sent)).await
    }

    fn check(&self) -> Result<()> {
        ensure!(self.pong.elapsed() < DEADLINE, "Peer heartbeat timed out");
        Ok(())
    }

    fn receive(&mut self, sequence: u64, state: &SharedState) {
        if sequence > self.received && sequence <= self.sent {
            self.received = sequence;
            self.pong = Instant::now();
            state.last_heartbeat_ms.store(now_ms(), Ordering::Release);
        }
    }
}

fn connected(state: &SharedState, connection: &Connection, features: Features) {
    state.set_peer(connection.remote_address().to_string());
    *state.features.lock().unwrap() = features;
    state.clear_error();
    state.connected.store(true, Ordering::Release);
}

async fn serve_peer(
    peer: Authenticated,
    local: ScreenInfo,
    edge: Edge,
    state: &SharedState,
    mut desired: watch::Receiver<Features>,
    mut cancelled: watch::Receiver<bool>,
    start_capture: impl FnOnce() -> Result<Capture>,
) -> Result<()> {
    let same_platform = peer.mac == cfg!(target_os = "macos");
    state
        .keyboard_supported
        .store(same_platform, Ordering::Release);
    let negotiate = |mut features: Features| {
        features.keyboard &= same_platform;
        features
    };
    let mut features = negotiate(*desired.borrow_and_update());
    let mut capture = start_capture()?;
    let mut tx = peer.tx;
    time::timeout(
        HEARTBEAT,
        transport::send(&mut tx, &LoginReply::Ready(features)),
    )
    .await??;
    let mut link = Channel::new(peer.connection, tx, peer.rx);
    let mut clipboard = clipboard::Sync::new(link.connection.clone(), true);
    clipboard.set_enabled(features.clipboard);
    connected(state, &link.connection, features);
    let mut forward = Forward::new(local, peer.screen, edge);
    let mut liveness = Liveness::new();
    let mut heartbeat = time::interval(HEARTBEAT);
    let mut snapshot = time::interval(SNAPSHOT);
    let mut frame = time::interval(FRAME);
    frame.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancelled.changed() => break,
            message = link.receive() => match message? {
                Control::Ping(seq) => link.send(Control::Pong(seq)).await?,
                Control::Pong(seq) => liveness.receive(seq, state),
                Control::Configured(value) => {
                    if value == features { *state.features.lock().unwrap() = value; }
                }
                Control::EnterAck(epoch) if forward.pending.is_some() && epoch == forward.position.epoch => {
                    forward.pending = None;
                    forward.active = true;
                    state.mouse_on_peer.store(true, Ordering::Release);
                    for event in std::mem::take(&mut forward.buffered) {
                        if !forward.active { break; }
                        forward_event(event, &mut forward, &mut capture, state, features, &mut link).await?;
                    }
                }
                _ => {}
            },
            _ = desired.changed() => {
                features = negotiate(*desired.borrow_and_update());
                if !features.keyboard {
                    forward.disable_keyboard();
                    capture.suppression.keyboard.store(false, Ordering::Release);
                }
                clipboard.set_enabled(features.clipboard);
                link.send(Control::Configure(features)).await?;
                if features.keyboard && (forward.active || forward.pending.is_some()) {
                    capture.suppression.keyboard.store(true, Ordering::Release);
                }
            }
            event = capture.events.recv() => {
                let event = event.context("Input capture stopped")?;
                if forward.active {
                    forward_event(event, &mut forward, &mut capture, state, features, &mut link).await?;
                } else if forward.pending.is_some() {
                    ensure!(forward.buffered.len() < MAX_PENDING_INPUT, "Input buffer overflow while entering peer");
                    forward.buffered.push(event);
                } else if let CapturedInput::Mouse { event: MouseEvent::Move { dx, dy }, abs_x, abs_y } = event {
                    if let Some(enter) = forward.entry(dx, dy, abs_x, abs_y) {
                        capture.suppress(true, features.keyboard);
                        link.send(enter).await?;
                    }
                }
            }
            _ = frame.tick() => {
                liveness.check()?;
                ensure!(!capture.suppression.failed.load(Ordering::Acquire), "Input capture queue overflow");
                ensure!(!forward.pending.is_some_and(|start| start.elapsed() >= ENTER_TIMEOUT), "Peer did not confirm cursor entry");
                if forward.motion_dirty && forward.active {
                    forward.motion_dirty = false;
                    let _ = link.connection.send_datagram(transport::encode_position(&forward.position).to_vec().into());
                }
            }
            _ = heartbeat.tick() => liveness.ping(&mut link).await?,
            _ = snapshot.tick() => if forward.active { link.send(forward.snapshot()).await?; }
        }
    }
    capture.suppress(false, false);
    if forward.active || forward.pending.is_some() {
        let _ = link.send(Control::Leave(forward.position.epoch)).await;
    }
    Ok(())
}

async fn forward_event(
    event: CapturedInput,
    forward: &mut Forward,
    capture: &mut Capture,
    state: &SharedState,
    features: Features,
    link: &mut Channel,
) -> Result<()> {
    if let Some(message) = forward.input(event, features.keyboard) {
        if matches!(message, Control::Leave(_)) {
            capture.suppress(false, false);
            state.mouse_on_peer.store(false, Ordering::Release);
        }
        link.send(message).await?;
    }
    state.last_event_ms.store(now_ms(), Ordering::Release);
    Ok(())
}

async fn client_login(
    devices: &Devices,
    server: &PairedDevice,
    screen: ScreenInfo,
) -> Result<(quinn::Endpoint, Channel, Features)> {
    let (endpoint, connection) = transport::connect(
        server
            .address
            .as_deref()
            .context("Saved device has no server address")?,
        server.certificate.clone(),
    )
    .await?;
    let token = devices.token(server.id)?;
    let (tx, rx, features) = time::timeout(HANDSHAKE, async {
        let (mut tx, mut rx) = connection.open_bi().await?;
        transport::send(
            &mut tx,
            &Login::Connect {
                id: devices.id()?,
                token: *token,
                screen,
                mac: cfg!(target_os = "macos"),
            },
        )
        .await?;
        match transport::receive(&mut rx).await? {
            LoginReply::Ready(features) => Ok((tx, rx, features)),
            LoginReply::Rejected(message) => bail!("{message}"),
            _ => bail!("Unexpected authentication reply"),
        }
    })
    .await
    .context("Authentication timed out")??;
    Ok((endpoint, Channel::new(connection, tx, rx), features))
}

async fn connect(
    devices: Devices,
    server: PairedDevice,
    state: &SharedState,
    mut commands: mpsc::Receiver<SessionCommand>,
) -> Result<()> {
    let screen = screen::get_screen_info()?;
    ensure!(screen.valid(), "Invalid local screen dimensions");
    let login = client_login(&devices, &server, screen);
    tokio::pin!(login);
    let (_endpoint, mut link, mut features) = loop {
        tokio::select! {
            result = &mut login => break result?,
            command = commands.recv() => match command {
                Some(SessionCommand::Stop) | None => return Ok(()),
                Some(SessionCommand::Revoke(peer)) if peer == server.id => {
                    devices.forget(peer)?;
                    return Ok(());
                }
                Some(SessionCommand::Revoke(peer)) => devices.forget(peer)?,
                _ => {}
            },
        }
    };
    connected(state, &link.connection, features);
    let mut clipboard = clipboard::Sync::new(link.connection.clone(), false);
    clipboard.set_enabled(features.clipboard);
    let mut input = RemoteInput::new(crate::input::simulate::create_simulator()?, screen);
    let mut heartbeat = time::interval(HEARTBEAT);
    let mut liveness = Liveness::new();
    let mut frame = time::interval(Duration::from_secs_f64(
        1.0 / screen::get_display_refresh_hz().clamp(30.0, 240.0),
    ));
    frame.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let connection = link.connection.clone();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(SessionCommand::Stop) | None => break,
                Some(SessionCommand::Revoke(peer)) if peer == server.id => {
                    input.leave()?;
                    devices.forget(peer)?;
                    break;
                }
                Some(SessionCommand::Revoke(peer)) => devices.forget(peer)?,
                _ => {}
            },
            message = link.receive() => match message? {
                Control::Ping(seq) => link.send(Control::Pong(seq)).await?,
                Control::Pong(seq) => liveness.receive(seq, state),
                Control::Enter(position) => {
                    input.enter(position)?;
                    state.mouse_on_peer.store(true, Ordering::Release);
                    link.send(Control::EnterAck(position.epoch)).await?;
                }
                Control::Leave(epoch) if epoch == input.epoch => {
                    input.leave()?;
                    state.mouse_on_peer.store(false, Ordering::Release);
                }
                Control::Configure(value) => {
                    if !value.keyboard { input.release_keys()?; }
                    clipboard.set_enabled(value.clipboard);
                    features = value;
                    *state.features.lock().unwrap() = value;
                    link.send(Control::Configured(value)).await?;
                }
                message => {
                    input.apply(message, features.keyboard)?;
                    state.last_event_ms.store(now_ms(), Ordering::Release);
                }
            },
            datagram = connection.read_datagram() => {
                if let Some(position) = transport::decode_position(&datagram?) {
                    input.datagram(position);
                }
            }
            _ = frame.tick() => {
                if state.shutdown.load(Ordering::Acquire) { break; }
                liveness.check()?;
                input.flush()?;
            }
            _ = heartbeat.tick() => {
                ensure!(devices.list()?.iter().any(|d| d.id == server.id && d.address.is_some()), "Saved server was removed");
                liveness.ping(&mut link).await?;
            }
        }
    }
    input.leave()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::devices::CredentialStore;
    use std::{collections::HashMap, sync::Mutex as StdMutex};

    #[derive(Default)]
    struct Vault(StdMutex<HashMap<String, Vec<u8>>>);

    impl CredentialStore for Vault {
        fn get(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
            Ok(Zeroizing::new(
                self.0
                    .lock()
                    .unwrap()
                    .get(key)
                    .context("Missing credential")?
                    .clone(),
            ))
        }
        fn put(&self, key: &str, value: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(key.into(), value.to_vec());
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    fn screen() -> ScreenInfo {
        ScreenInfo {
            width: 1280,
            height: 720,
        }
    }

    async fn invitation(state: &SharedState) -> Invite {
        time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(code) = state.invitation.lock().unwrap().as_ref() {
                    return Invite::decode(code).unwrap();
                }
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn pair_restart_replace_and_revoke() -> Result<()> {
        let server_dir = tempfile::tempdir()?;
        let client_dir = tempfile::tempdir()?;
        let server_vault = Arc::new(Vault::default());
        let client_vault = Arc::new(Vault::default());
        let server = Devices::new(server_dir.path().into(), server_vault.clone());
        let client = Devices::new(client_dir.path().into(), client_vault.clone());
        server.set_features(Features {
            keyboard: true,
            clipboard: false,
        })?;
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = mpsc::channel(8);
        let state = shared.clone();
        let store = server.clone();
        let task =
            tokio::spawn(
                async move { serve(store, 0, Edge::Right, &state, rx, Capture::mock).await },
            );
        tx.send(SessionCommand::Invite("127.0.0.1".into())).await?;
        let code = invitation(&shared).await;

        let mut wrong = code.clone();
        wrong.token[0] ^= 1;
        assert!(pair(client.clone(), wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("Invitation invalid"));
        assert!(client.list()?.is_empty());

        let saved = pair(client.clone(), code.clone()).await?;
        let stranger_dir = tempfile::tempdir()?;
        let stranger = Devices::new(stranger_dir.path().into(), Arc::new(Vault::default()));
        assert!(pair(stranger.clone(), code.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("Invitation invalid"));
        stranger.add(saved.clone(), &[5; 32])?;
        assert!(client_login(&stranger, &saved, screen())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("not authorized"));

        let reopened = Devices::new(client_dir.path().into(), client_vault);
        let reopened_server = Devices::new(server_dir.path().into(), server_vault);
        assert_eq!(reopened_server.identity()?.1, saved.certificate);
        tx.send(SessionCommand::Stop).await?;
        task.await??;
        let (tx, rx) = mpsc::channel(8);
        let shared = Arc::new(SharedState::new());
        let state = shared.clone();
        let store = reopened_server.clone();
        let task =
            tokio::spawn(
                async move { serve(store, 0, Edge::Right, &state, rx, Capture::mock).await },
            );
        tx.send(SessionCommand::Invite("127.0.0.1".into())).await?;
        let address = invitation(&shared).await.server.address.unwrap();
        reopened.set_address(saved.id, address)?;
        let saved = reopened
            .list()?
            .into_iter()
            .find(|d| d.id == saved.id)
            .unwrap();
        let (ep1, first, _) = client_login(&reopened, &saved, screen()).await?;
        let (ep2, mut second, _) = client_login(
            &reopened,
            &saved,
            ScreenInfo {
                width: 1920,
                height: 1080,
            },
        )
        .await?;
        assert_ne!(ep1.local_addr()?.port(), ep2.local_addr()?.port());
        time::timeout(HEARTBEAT, first.connection.closed()).await?;
        drop(first);
        // Clipboard stays disabled and authenticated control remains usable.
        second.send(Control::Ping(99)).await?;
        time::timeout(HEARTBEAT, async {
            while !matches!(second.receive().await?, Control::Pong(99)) {}
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        tx.send(SessionCommand::Features {
            keyboard: Some(false),
            clipboard: None,
        })
        .await?;
        let new_features = time::timeout(HEARTBEAT, async {
            loop {
                if let Control::Configure(value) = second.receive().await? {
                    return Ok::<_, anyhow::Error>(value);
                }
            }
        })
        .await??;
        assert!(!new_features.keyboard);
        assert!(
            shared.status().features.keyboard,
            "UI still shows the last confirmed setting"
        );
        second.send(Control::Configured(new_features)).await?;
        time::timeout(HEARTBEAT, async {
            while shared.status().features.keyboard {
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(reopened_server.features()?, new_features);

        tx.send(SessionCommand::Revoke(reopened.id()?)).await?;
        time::timeout(HEARTBEAT, second.connection.closed()).await?;
        assert!(client_login(&reopened, &saved, screen()).await.is_err());
        tx.send(SessionCommand::Stop).await?;
        task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn expired_invitation_and_changed_identity_are_rejected() -> Result<()> {
        let server_dir = tempfile::tempdir()?;
        let client_dir = tempfile::tempdir()?;
        let server = Devices::new(server_dir.path().into(), Arc::new(Vault::default()));
        let client = Devices::new(client_dir.path().into(), Arc::new(Vault::default()));
        let (id, cert, key) = server.identity()?;
        let endpoint = transport::server("127.0.0.1:0".parse()?, cert.clone(), key.to_vec())?;
        let code = Invite {
            server: PairedDevice {
                id,
                name: "test".into(),
                address: Some(endpoint.local_addr()?.to_string()),
                certificate: cert,
            },
            token: [7; 32],
        };
        let lease = Arc::new(Mutex::new(Some(Invitation {
            token: Zeroizing::new(code.token),
            expires: Instant::now(),
        })));
        let listener = endpoint.clone();
        let task = tokio::spawn(async move {
            authenticate(listener.accept().await.unwrap(), server, lease)
                .await
                .map(|_| ())
        });
        let err = pair(client.clone(), code.clone()).await.unwrap_err();
        assert!(err.to_string().contains("expired"));
        task.await??;
        let (_, wrong_cert, _) = client.identity()?;
        let address = code.server.address.as_deref().unwrap();
        for (certificate, alpn) in [
            (wrong_cert, transport::ALPN),
            (code.server.certificate.clone(), b"mouse-share/1".as_slice()),
        ] {
            let (client, server) = tokio::join!(
                transport::connect_with_alpn(address, certificate, alpn),
                async { endpoint.accept().await.unwrap().await },
            );
            assert!(client.is_err());
            assert!(server.is_err());
        }
        assert!(client.list()?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn live_local_input_does_not_hold_suppression_after_peer_stops_responding() -> Result<()>
    {
        let cert =
            rcgen::generate_simple_self_signed(vec![super::super::devices::TLS_NAME.into()])?;
        let der = cert.cert.der().to_vec();
        let endpoint = transport::server(
            "127.0.0.1:0".parse()?,
            der.clone(),
            cert.signing_key.serialize_der(),
        )?;
        let address = endpoint.local_addr()?.to_string();
        let (client, server) = tokio::join!(transport::connect(&address, der), async {
            endpoint.accept().await.unwrap().await
        },);
        let (_client_endpoint, client) = client?;
        let server = server?;
        let (mut tx, mut rx) = client.open_bi().await?;
        tx.write_all(&[1]).await?;
        let (server_tx, mut server_rx) = server.accept_bi().await?;
        let mut header = [0];
        server_rx.read_exact(&mut header).await?;
        let state = Arc::new(SharedState::new());
        let server_state = state.clone();
        let (_config, features) = watch::channel(Features {
            keyboard: true,
            clipboard: false,
        });
        let (_stop, stop) = watch::channel(false);
        let (capture, events) = Capture::mock_with_input();
        let suppression = capture.suppression.clone();
        let task = tokio::spawn(async move {
            serve_peer(
                Authenticated {
                    connection: server,
                    tx: server_tx,
                    rx: server_rx,
                    peer: [0; 16],
                    screen: screen(),
                    mac: cfg!(target_os = "macos"),
                },
                screen(),
                Edge::Right,
                &server_state,
                features,
                stop,
                || Ok(capture),
            )
            .await
        });
        assert!(matches!(
            transport::receive::<LoginReply>(&mut rx).await?,
            LoginReply::Ready(_)
        ));
        let movement = CapturedInput::Mouse {
            event: MouseEvent::Move { dx: 1.0, dy: 0.0 },
            abs_x: f64::from(screen().width - 1),
            abs_y: 100.0,
        };
        events.send(movement.clone()).await?;
        let epoch = time::timeout(HEARTBEAT, async {
            loop {
                if let Control::Enter(position) = transport::receive(&mut rx).await? {
                    return Ok::<_, anyhow::Error>(position.epoch);
                }
            }
        })
        .await??;
        transport::send(&mut tx, &Control::EnterAck(epoch)).await?;
        time::timeout(HEARTBEAT, async {
            while !state.mouse_on_peer.load(Ordering::Acquire) {
                time::sleep(FRAME).await;
            }
        })
        .await?;
        assert!(suppression.mouse.load(Ordering::Acquire));
        assert!(suppression.keyboard.load(Ordering::Acquire));
        // QUIC stays connected at the transport layer, but the remote app
        // sends no Pongs. Keep generating local input until cleanup runs.
        let producer = tokio::spawn(async move {
            while events.send(movement.clone()).await.is_ok() {
                time::sleep(Duration::from_millis(20)).await;
            }
        });
        let failure = time::timeout(DEADLINE + HEARTBEAT, task)
            .await??
            .unwrap_err();
        assert!(failure.to_string().contains("heartbeat"));
        assert!(!suppression.mouse.load(Ordering::Acquire));
        assert!(!suppression.keyboard.load(Ordering::Acquire));
        producer.await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_expires_only_without_new_valid_pongs() {
        let mut live = Liveness::new();
        let state = SharedState::new();
        for seq in 1..=10 {
            time::advance(HEARTBEAT).await;
            live.sent = seq;
            live.receive(seq, &state);
            assert!(live.check().is_ok());
        }
        for _ in 0..5 {
            time::advance(HEARTBEAT).await;
            state.last_event_ms.store(now_ms(), Ordering::Release);
            live.receive(10, &state);
            live.receive(100, &state);
        }
        assert!(live.check().is_err());
    }
}
