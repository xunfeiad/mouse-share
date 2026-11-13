//! Session owner: all control transitions happen on one worker. OS callbacks
//! only capture input; UI operations only change desired configuration.
use super::{
    session::{Role, SessionConfig},
    state::{Phase, SharedState},
    transport::{self, Framed},
};
use crate::{
    clipboard::{ClipboardBackend, ClipboardWorker},
    config::{Edge, ScreenConfig},
    input::{
        capture::{self, CaptureControl, CapturedInput, InputCapture},
        pressed::PressedInputState,
        simulate::{self, InputSimulator},
    },
    protocol::{
        self, Control, ControlFrame, Features, Motion, MouseButton, MouseEvent, Position,
        ScreenInfo, SessionId,
    },
};
use anyhow::{bail, ensure, Result};
use std::{
    collections::HashSet,
    net::{SocketAddr, TcpListener, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const TICK: Duration = Duration::from_millis(2);
const TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_LIMIT: usize = 1024 * 1024;

pub trait Backend: Send + Sync {
    fn screen(&self) -> Result<ScreenInfo>;
    fn capture(&self) -> Box<dyn InputCapture>;
    fn simulator(&self) -> Box<dyn InputSimulator>;
    fn cursor(&self, hidden: bool, restore: bool);
    fn clipboard(&self) -> Result<Box<dyn ClipboardBackend>>;
    fn inputs_held(&self) -> bool {
        false
    }
    fn refresh_hz(&self) -> f64 {
        60.
    }
}
pub struct NativeBackend;
impl Backend for NativeBackend {
    fn screen(&self) -> Result<ScreenInfo> {
        crate::screen::get_screen_info()
    }
    fn capture(&self) -> Box<dyn InputCapture> {
        capture::create_capture()
    }
    fn simulator(&self) -> Box<dyn InputSimulator> {
        simulate::create_simulator()
    }
    fn cursor(&self, hidden: bool, restore: bool) {
        if hidden {
            capture::hide_local_cursor();
        } else if restore {
            capture::show_local_cursor();
        } else {
            capture::show_local_cursor_no_restore();
        }
    }
    fn clipboard(&self) -> Result<Box<dyn ClipboardBackend>> {
        crate::clipboard::NativeClipboard::open()
    }
    fn inputs_held(&self) -> bool {
        capture::physical_input_held()
    }
    fn refresh_hz(&self) -> f64 {
        crate::screen::get_display_refresh_hz()
    }
}
struct CursorGuard {
    backend: Arc<dyn Backend>,
    hidden: bool,
}
impl CursorGuard {
    fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            hidden: false,
        }
    }
    fn hide(&mut self) {
        if !self.hidden {
            self.backend.cursor(true, false);
            self.hidden = true;
        }
    }
    fn show(&mut self, restore: bool) {
        if self.hidden {
            self.backend.cursor(false, restore);
            self.hidden = false;
        }
    }
}
impl Drop for CursorGuard {
    fn drop(&mut self) {
        self.show(true);
    }
}
struct CaptureWorker {
    control: Arc<CaptureControl>,
    stop: Arc<AtomicBool>,
    events: crossbeam_channel::Receiver<CapturedInput>,
    outcome: crossbeam_channel::Receiver<Result<()>>,
    thread: Option<JoinHandle<()>>,
}
impl CaptureWorker {
    fn start(backend: &dyn Backend) -> Result<Self> {
        let mut capture = backend.capture();
        let control = capture.control_handle();
        let (tx, events) = crossbeam_channel::bounded(4096);
        let (done, outcome) = crossbeam_channel::bounded(1);
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        let thread = std::thread::Builder::new()
            .name("input-capture".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    capture.run(tx, cancelled)
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Input capture panicked")));
                let _ = done.send(result);
            })?;
        Ok(Self {
            control,
            stop,
            events,
            outcome,
            thread: Some(thread),
        })
    }
    fn check(&self) -> Result<()> {
        ensure!(
            !self.control.fault.load(Ordering::SeqCst),
            "Input capture overflowed or was disabled; local input restored"
        );
        if let Ok(result) = self.outcome.try_recv() {
            result?;
            bail!("Input capture stopped");
        }
        Ok(())
    }
}
impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.control.release();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
fn clip(
    addr: SocketAddr,
    server: bool,
    id: SessionId,
    epoch: u64,
    state: &Arc<SharedState>,
    backend: &Arc<dyn Backend>,
) -> Result<ClipboardWorker> {
    let factory_backend = backend.clone();
    state.clipboard_error(None);
    ClipboardWorker::start(
        addr,
        server,
        id,
        epoch,
        state.clone(),
        Arc::new(move || factory_backend.clipboard()),
    )
}
fn stopped(state: &SharedState) -> bool {
    state.shutdown.load(Ordering::SeqCst)
}
fn pause() {
    std::thread::sleep(TICK);
}
fn queue(wire: &mut Framed, id: SessionId, seq: u64, message: Control) -> Result<()> {
    wire.queue(&ControlFrame::new(id, seq, message))
}
fn valid(frame: &ControlFrame) -> Result<()> {
    ensure!(
        frame.magic == protocol::MAGIC,
        "Protocol mismatch: upgrade both computers"
    );
    Ok(())
}
fn finish_wire(wire: &mut Framed, id: SessionId) {
    let _ = queue(wire, id, 0, Control::Goodbye);
    let deadline = Instant::now() + Duration::from_millis(50);
    while wire.pending() && Instant::now() < deadline {
        if wire.flush().is_err() {
            break;
        }
        pause();
    }
}

pub fn run(
    config: SessionConfig,
    state: Arc<SharedState>,
    backend: Arc<dyn Backend>,
) -> Result<()> {
    if config.role == Role::Server {
        run_server(config, state, backend)
    } else {
        run_client(config, state, backend)
    }
}

struct Peer {
    wire: Framed,
    id: SessionId,
    device: SessionId,
    screen: ScreenInfo,
    addr: SocketAddr,
    available: bool,
    ready: bool,
    rx: Instant,
    hb: Instant,
    created: Instant,
    ack_since: Instant,
    seq: u64,
    applied: u64,
    features: Features,
    remote_features: Features,
    epoch: u64,
    settings_pending: bool,
    clipboard: Option<ClipboardWorker>,
}
impl Peer {
    fn send(&mut self, message: Control) -> Result<()> {
        if self.seq == self.applied {
            self.ack_since = Instant::now();
        }
        self.seq += 1;
        queue(&mut self.wire, self.id, self.seq, message)
    }
}
struct Forwarder {
    remote: bool,
    generation: u64,
    position: Position,
    armed: bool,
    keys: HashSet<u32>,
    buttons: HashSet<MouseButton>,
}
impl Forwarder {
    fn new() -> Self {
        Self {
            remote: false,
            generation: 0,
            position: Position::default(),
            armed: false,
            keys: HashSet::new(),
            buttons: HashSet::new(),
        }
    }
    fn release(&mut self, capture: &CaptureControl, cursor: &mut CursorGuard, state: &SharedState) {
        capture.release();
        cursor.show(true);
        state.ownership(false);
        self.remote = false;
    }
    fn event(
        &mut self,
        captured: CapturedInput,
        peer: &mut Peer,
        config: &ScreenConfig,
        control: &CaptureControl,
        cursor: &mut CursorGuard,
        state: &SharedState,
        backend: &dyn Backend,
    ) -> Result<bool> {
        match captured {
            CapturedInput::Key { event, remote } => {
                if event.down {
                    self.keys.insert(event.keycode);
                } else {
                    self.keys.remove(&event.keycode);
                }
                if self.remote && remote && peer.features.keyboard {
                    peer.send(Control::Key {
                        generation: self.generation,
                        position: self.position,
                        event,
                    })?;
                }
            }
            CapturedInput::Mouse {
                event,
                abs_x,
                abs_y,
                remote,
            } => {
                match &event {
                    MouseEvent::ButtonDown(b) => {
                        self.buttons.insert(*b);
                    }
                    MouseEvent::ButtonUp(b) => {
                        self.buttons.remove(b);
                    }
                    _ => (),
                }
                if !self.remote {
                    if let MouseEvent::Move { dx, dy } = event {
                        let outward = match config.edge {
                            Edge::Right => dx > 0.,
                            Edge::Left => dx < 0.,
                            Edge::Bottom => dy > 0.,
                            Edge::Top => dy < 0.,
                        };
                        if outward
                            && config.at_edge(abs_x, abs_y)
                            && self.keys.is_empty()
                            && self.buttons.is_empty()
                            && !backend.inputs_held()
                        {
                            let (x, y) = config.entry_position(abs_x, abs_y);
                            self.generation += 1;
                            self.position = Position { x, y, motion: 0 };
                            self.armed = false;
                            peer.send(Control::Enter {
                                generation: self.generation,
                                position: self.position,
                            })?;
                            self.remote = true;
                            control.mouse.store(true, Ordering::SeqCst);
                            control
                                .keyboard
                                .store(peer.features.keyboard, Ordering::SeqCst);
                            cursor.hide();
                            state.ownership(true);
                        }
                    }
                } else if remote {
                    if let MouseEvent::Move { dx, dy } = event {
                        if !dx.is_finite() || !dy.is_finite() {
                            bail!("Invalid captured movement");
                        }
                        self.position.x =
                            (self.position.x + dx).clamp(0., peer.screen.width as f64 - 1.);
                        self.position.y =
                            (self.position.y + dy).clamp(0., peer.screen.height as f64 - 1.);
                        self.position.motion += 1;
                        let inside = match config.edge {
                            Edge::Right => self.position.x,
                            Edge::Left => peer.screen.width as f64 - 1. - self.position.x,
                            Edge::Bottom => self.position.y,
                            Edge::Top => peer.screen.height as f64 - 1. - self.position.y,
                        };
                        if inside >= 20. {
                            self.armed = true;
                        }
                        if self.armed && inside <= 0. {
                            peer.send(Control::Leave {
                                generation: self.generation,
                            })?;
                            self.release(control, cursor, state);
                        } else {
                            return Ok(true);
                        }
                    } else {
                        peer.send(Control::Input {
                            generation: self.generation,
                            position: self.position,
                            event,
                        })?;
                    }
                }
            }
        }
        Ok(false)
    }
}

fn run_server(
    config: SessionConfig,
    state: Arc<SharedState>,
    backend: Arc<dyn Backend>,
) -> Result<()> {
    let bind = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = TcpListener::bind(bind)?;
    listener.set_nonblocking(true)?;
    let udp = UdpSocket::bind(bind)?;
    udp.set_nonblocking(true)?;
    let screen = backend.screen()?;
    ensure!(protocol::valid_screen(&screen), "Invalid screen size");
    let capture = CaptureWorker::start(&*backend)?;
    let startup = Instant::now();
    while !capture.control.ready.load(Ordering::SeqCst) && !stopped(&state) {
        capture.check()?;
        ensure!(
            startup.elapsed() < Duration::from_secs(3),
            "Input capture initialization timed out"
        );
        pause();
    }
    let mut cursor = CursorGuard::new(backend.clone());
    let mut forward = Forwarder::new();
    let mut peer: Option<Peer> = None;
    let mut pending: Vec<(Framed, SocketAddr, Instant)> = Vec::new();
    state.phase(Phase::Listening);
    while !stopped(&state) {
        capture.check()?;
        // Accept and parse handshakes without blocking the active peer.
        for _ in 0..8 {
            match listener.accept() {
                Ok((s, addr)) if pending.len() < 8 => {
                    pending.push((Framed::new(s, CONTROL_LIMIT)?, addr, Instant::now()))
                }
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        let mut i = 0;
        while i < pending.len() {
            let incoming = pending[i].0.recv::<ControlFrame>();
            let remove =
                !matches!(&incoming, Ok(None)) || pending[i].2.elapsed() > Duration::from_secs(2);
            if !remove {
                i += 1;
                continue;
            }
            let (mut wire, addr, _) = pending.swap_remove(i);
            if let Ok(Some(frame)) = incoming {
                let parsed = match frame.message {
                    Control::Hello {
                        version,
                        device,
                        platform,
                        screen,
                        udp_port,
                        features,
                    } if frame.magic == protocol::MAGIC
                        && version == protocol::VERSION
                        && protocol::valid_screen(&screen)
                        && udp_port != 0 =>
                    {
                        Some((device, platform, screen, udp_port, features))
                    }
                    _ => None,
                };
                if let Some((device, platform, client_screen, udp_port, remote_features)) = parsed {
                    if peer.as_ref().is_some_and(|p| p.device != device) {
                        let _ = queue(
                            &mut wire,
                            [0; 16],
                            0,
                            Control::Reject("Another device is connected".into()),
                        );
                        let _ = wire.flush();
                        continue;
                    }
                    forward.release(&capture.control, &mut cursor, &state);
                    state.settings_pending(false);
                    state.phase(Phase::Listening);
                    if let Some(mut old) = peer.take() {
                        finish_wire(&mut old.wire, old.id);
                        drop(old);
                    }
                    let available = platform == protocol::platform();
                    let mut features = *state.desired.lock().unwrap();
                    features.keyboard &= available && remote_features.keyboard;
                    features.clipboard &= remote_features.clipboard;
                    let id = protocol::new_id();
                    let epoch = 1;
                    let clipboard = if features.clipboard {
                        match clip(
                            SocketAddr::from(([0, 0, 0, 0], config.port + 1)),
                            true,
                            id,
                            epoch,
                            &state,
                            &backend,
                        ) {
                            Ok(c) => Some(c),
                            Err(e) => {
                                state.clipboard_error(Some(e.to_string()));
                                features.clipboard = false;
                                state.desired.lock().unwrap().clipboard = false;
                                None
                            }
                        }
                    } else {
                        None
                    };
                    queue(
                        &mut wire,
                        id,
                        0,
                        Control::Welcome {
                            version: protocol::VERSION,
                            session: id,
                            platform: protocol::platform(),
                            screen: screen.clone(),
                            edge: config.edge,
                            features,
                            clipboard_epoch: epoch,
                        },
                    )?;
                    state.set_peer(SocketAddr::new(addr.ip(), udp_port).to_string());
                    peer = Some(Peer {
                        wire,
                        id,
                        device,
                        screen: client_screen,
                        addr: SocketAddr::new(addr.ip(), udp_port),
                        available,
                        ready: false,
                        rx: Instant::now(),
                        hb: Instant::now(),
                        created: Instant::now(),
                        ack_since: Instant::now(),
                        seq: 0,
                        applied: 0,
                        features,
                        remote_features,
                        epoch,
                        settings_pending: false,
                        clipboard,
                    });
                } else {
                    let _ = queue(
                        &mut wire,
                        [0; 16],
                        0,
                        Control::Reject("Protocol mismatch: upgrade both computers".into()),
                    );
                    let _ = wire.flush();
                }
            }
        }
        if let Some(p) = peer.as_mut() {
            let outcome = (|| -> Result<()> {
                p.wire.flush()?;
                for _ in 0..64 {
                    let Some(frame) = p.wire.recv::<ControlFrame>()? else {
                        break;
                    };
                    valid(&frame)?;
                    if frame.session != p.id {
                        continue;
                    }
                    match frame.message {
                        Control::Ready if !p.ready => {
                            p.ready = true;
                            state.clear_error();
                            state.features(p.features, p.available, config.edge);
                            state.phase(Phase::Connected);
                        }
                        Control::Heartbeat => (),
                        Control::Preferences(features) => p.remote_features = features,
                        Control::Applied(seq) if seq <= p.seq => {
                            if seq > p.applied {
                                p.applied = seq;
                                p.ack_since = Instant::now();
                            }
                        }
                        Control::SettingsAck {
                            features,
                            clipboard_epoch,
                        } if p.settings_pending
                            && clipboard_epoch == p.epoch
                            && features == p.features =>
                        {
                            p.settings_pending = false;
                            state.settings_pending(false);
                            state.features(p.features, p.available, config.edge);
                            capture
                                .control
                                .keyboard
                                .store(forward.remote && p.features.keyboard, Ordering::SeqCst);
                        }
                        Control::ClipboardFault(error) => {
                            state.clipboard_error(Some(error));
                            state.desired.lock().unwrap().clipboard = false;
                        }
                        Control::Goodbye => bail!("Peer disconnected"),
                        _ => bail!("Unexpected control message"),
                    }
                    p.rx = Instant::now();
                    state
                        .last_heartbeat_ms
                        .store(super::state::now_ms(), Ordering::SeqCst);
                }
                ensure!(p.rx.elapsed() < TIMEOUT, "Peer heartbeat timeout");
                if !p.ready {
                    ensure!(
                        p.created.elapsed() < Duration::from_secs(2),
                        "Handshake timeout"
                    );
                    return Ok(());
                }
                ensure!(
                    p.seq == p.applied || p.ack_since.elapsed() < Duration::from_secs(2),
                    "Peer did not acknowledge input; local input restored"
                );
                if p.hb.elapsed() >= Duration::from_secs(1) {
                    queue(&mut p.wire, p.id, 0, Control::Heartbeat)?;
                    p.hb = Instant::now();
                }
                if p.clipboard.as_ref().is_some_and(|c| c.is_finished()) {
                    state.desired.lock().unwrap().clipboard = false;
                }
                let mut desired = *state.desired.lock().unwrap();
                desired.keyboard &= p.available && p.remote_features.keyboard;
                desired.clipboard &= p.remote_features.clipboard;
                if !p.settings_pending && desired != p.features {
                    if desired.clipboard != p.features.clipboard {
                        p.clipboard.take();
                        p.epoch += 1;
                        if desired.clipboard {
                            match clip(
                                SocketAddr::from(([0, 0, 0, 0], config.port + 1)),
                                true,
                                p.id,
                                p.epoch,
                                &state,
                                &backend,
                            ) {
                                Ok(c) => p.clipboard = Some(c),
                                Err(e) => {
                                    desired.clipboard = false;
                                    state.desired.lock().unwrap().clipboard = false;
                                    state.clipboard_error(Some(e.to_string()));
                                }
                            }
                        }
                    }
                    p.features = desired;
                    p.settings_pending = true;
                    state.settings_pending(true);
                    p.send(Control::Settings {
                        features: desired,
                        clipboard_epoch: p.epoch,
                    })?;
                }
                let mut screen_config = ScreenConfig::new(screen.clone(), config.edge);
                screen_config.client_screen = Some(p.screen.clone());
                let mut moved = false;
                for _ in 0..256 {
                    let Ok(event) = capture.events.try_recv() else {
                        break;
                    };
                    moved |= forward.event(
                        event,
                        p,
                        &screen_config,
                        &capture.control,
                        &mut cursor,
                        &state,
                        &*backend,
                    )?;
                }
                if moved && forward.remote {
                    let motion = Motion {
                        magic: protocol::MAGIC,
                        session: p.id,
                        generation: forward.generation,
                        after_control: p.seq,
                        position: forward.position,
                    };
                    // No retry: next absolute position supersedes a lost move.
                    let _ = udp.send_to(&bincode::serialize(&motion)?, p.addr);
                }
                p.wire.flush()?;
                Ok(())
            })();
            if let Err(e) = outcome {
                forward.release(&capture.control, &mut cursor, &state);
                state.set_error(e.to_string());
                state.phase(Phase::Listening);
                state.settings_pending(false);
                peer.take();
            }
        } else {
            // Keep physical held-state up to date while listening.
            for _ in 0..256 {
                match capture.events.try_recv() {
                    Ok(CapturedInput::Key { event, .. }) => {
                        if event.down {
                            forward.keys.insert(event.keycode);
                        } else {
                            forward.keys.remove(&event.keycode);
                        }
                    }
                    Ok(CapturedInput::Mouse {
                        event: MouseEvent::ButtonDown(b),
                        ..
                    }) => {
                        forward.buttons.insert(b);
                    }
                    Ok(CapturedInput::Mouse {
                        event: MouseEvent::ButtonUp(b),
                        ..
                    }) => {
                        forward.buttons.remove(&b);
                    }
                    Ok(_) => (),
                    Err(_) => break,
                }
            }
        }
        // Legacy UDP Hello never creates a session or suppresses input.
        let mut buf = [0; 4096];
        for _ in 0..8 {
            match udp.recv_from(&mut buf) {
                Ok((n, _))
                    if matches!(
                        protocol::deserialize(&buf[..n]),
                        Ok(protocol::Message::Hello(_))
                    ) =>
                {
                    state.set_error("Old protocol detected: upgrade both computers")
                }
                Ok(_) => (),
                Err(_) => break,
            }
        }
        pause();
    }
    forward.release(&capture.control, &mut cursor, &state);
    if let Some(mut p) = peer {
        finish_wire(&mut p.wire, p.id);
    }
    Ok(())
}

/// Receiver ordering can be exercised without any OS injection or sockets.
pub struct ReceiverState {
    pub input: PressedInputState,
    pub active: bool,
    pub generation: u64,
    pub control: u64,
    pub motion: u64,
    pending: Option<Motion>,
    width: f64,
    height: f64,
}
impl ReceiverState {
    pub fn new(simulator: Box<dyn InputSimulator>, screen: &ScreenInfo) -> Self {
        Self {
            input: PressedInputState::new(simulator),
            active: false,
            generation: 0,
            control: 0,
            motion: 0,
            pending: None,
            width: screen.width as f64,
            height: screen.height as f64,
        }
    }
    fn position(&mut self, position: Position) -> Result<()> {
        ensure!(
            position.x.is_finite() && position.y.is_finite(),
            "Invalid cursor position"
        );
        self.input.simulator.move_to(
            position.x.clamp(0., self.width - 1.),
            position.y.clamp(0., self.height - 1.),
        )?;
        self.motion = position.motion;
        Ok(())
    }
    pub fn motion(&mut self, packet: Motion) -> Result<()> {
        if packet.generation < self.generation {
            return Ok(());
        }
        if packet.after_control > self.control {
            if self.pending.as_ref().is_none_or(|p| {
                (packet.generation, packet.position.motion) > (p.generation, p.position.motion)
            }) {
                self.pending = Some(packet);
            }
        } else if self.active
            && packet.generation == self.generation
            && packet.position.motion > self.motion
        {
            self.position(packet.position)?;
        }
        Ok(())
    }
    fn apply_pending(&mut self) -> Result<()> {
        if let Some(p) = self.pending.take() {
            self.motion(p)?;
        }
        Ok(())
    }
}

fn run_client(
    config: SessionConfig,
    state: Arc<SharedState>,
    backend: Arc<dyn Backend>,
) -> Result<()> {
    let addr = transport::parse_address(&config.server)?;
    state.set_peer(addr.to_string());
    let mut delay = 1u64;
    loop {
        if stopped(&state) {
            return Ok(());
        }
        state.phase(if delay == 1 {
            Phase::Connecting
        } else {
            Phase::Reconnecting
        });
        let result = client_connection(&config, addr, &state, &backend);
        state.ownership(false);
        if stopped(&state) {
            return Ok(());
        }
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                let established = state.snapshot().phase == Phase::Connected;
                state.set_error(format!("{e:#}"));
                if !state.auto_reconnect.load(Ordering::SeqCst)
                    || e.to_string().contains("Protocol mismatch")
                {
                    return Err(e);
                }
                if established {
                    delay = 1;
                }
                state.phase(Phase::Reconnecting);
                let jitter = protocol::new_id()[0] as u64 % 150;
                let deadline = Instant::now() + Duration::from_millis(delay * 1000 + jitter);
                while Instant::now() < deadline {
                    if stopped(&state) {
                        return Ok(());
                    }
                    if !state.auto_reconnect.load(Ordering::SeqCst) {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                delay = (delay * 2).min(8);
            }
        }
    }
}
fn client_connection(
    config: &SessionConfig,
    addr: SocketAddr,
    state: &Arc<SharedState>,
    backend: &Arc<dyn Backend>,
) -> Result<()> {
    let udp = UdpSocket::bind(if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })?;
    udp.set_nonblocking(true)?;
    let screen = backend.screen()?;
    ensure!(protocol::valid_screen(&screen), "Invalid screen");
    let stream = match transport::connect(addr, &state.shutdown, Duration::from_secs(2)) {
        Ok(s) => s,
        Err(e) => {
            if stopped(state) {
                return Ok(());
            }
            let _ = udp.send_to(
                &protocol::serialize(&protocol::Message::Hello(screen.clone()))?,
                addr,
            );
            let deadline = Instant::now() + Duration::from_millis(150);
            let mut buf = [0; 4096];
            while Instant::now() < deadline && !stopped(state) {
                if let Ok((n, source)) = udp.recv_from(&mut buf) {
                    if source == addr
                        && matches!(
                            protocol::deserialize(&buf[..n]),
                            Ok(protocol::Message::HelloAck(_))
                        )
                    {
                        bail!("Protocol mismatch: upgrade both computers");
                    }
                }
                pause();
            }
            return Err(e);
        }
    };
    let mut wire = Framed::new(stream, CONTROL_LIMIT)?;
    let mut sent_preferences = *state.desired.lock().unwrap();
    queue(
        &mut wire,
        [0; 16],
        0,
        Control::Hello {
            version: protocol::VERSION,
            device: config.device,
            platform: protocol::platform(),
            screen: screen.clone(),
            udp_port: udp.local_addr()?.port(),
            features: sent_preferences,
        },
    )?;
    let start = Instant::now();
    let (id, available, edge, mut features, mut epoch) = loop {
        if stopped(state) {
            return Ok(());
        }
        wire.flush()?;
        if let Some(frame) = wire.recv::<ControlFrame>()? {
            valid(&frame)?;
            match frame.message {
                Control::Welcome {
                    version,
                    session,
                    platform,
                    screen,
                    edge,
                    features,
                    clipboard_epoch,
                } => {
                    ensure!(
                        version == protocol::VERSION
                            && protocol::valid_screen(&screen)
                            && frame.session == session,
                        "Protocol mismatch: upgrade both computers"
                    );
                    break (
                        session,
                        platform == protocol::platform(),
                        edge,
                        features,
                        clipboard_epoch,
                    );
                }
                Control::Reject(reason) => bail!("{reason}"),
                _ => bail!("Invalid handshake"),
            }
        }
        ensure!(
            start.elapsed() < Duration::from_secs(2),
            "Handshake timeout"
        );
        pause();
    };
    let mut receiver = ReceiverState::new(backend.simulator(), &screen);
    let mut cursor = CursorGuard::new(backend.clone());
    cursor.hide();
    let clip_addr = SocketAddr::new(addr.ip(), addr.port() + 1);
    let mut clipboard = if features.clipboard {
        Some(clip(clip_addr, false, id, epoch, state, backend)?)
    } else {
        None
    };
    queue(&mut wire, id, 0, Control::Ready)?;
    state.clear_error();
    state.features(features, available, edge);
    state.phase(Phase::Connected);
    let mut rx = Instant::now();
    let mut hb = Instant::now();
    // Preserve display-paced cursor injection: retain the latest absolute
    // move while TCP clicks/keys are applied immediately at their own position.
    let refresh_hz = backend.refresh_hz();
    let frame_interval = Duration::from_secs_f64(
        1. / if refresh_hz.is_finite() {
            refresh_hz.clamp(30., 240.)
        } else {
            60.
        },
    );
    let mut next_move = Instant::now();
    let mut latest_move: Option<Motion> = None;
    let result = (|| -> Result<()> {
        while !stopped(state) {
            let desired = *state.desired.lock().unwrap();
            if desired != sent_preferences {
                if !desired.keyboard {
                    receiver.input.release_keys();
                }
                if !desired.clipboard {
                    clipboard.take();
                }
                queue(&mut wire, id, 0, Control::Preferences(desired))?;
                sent_preferences = desired;
            }
            wire.flush()?;
            for _ in 0..64 {
                let Some(frame) = wire.recv::<ControlFrame>()? else {
                    break;
                };
                valid(&frame)?;
                if frame.session != id {
                    continue;
                }
                if frame.sequence > 0 {
                    ensure!(
                        frame.sequence == receiver.control + 1,
                        "Invalid control sequence"
                    );
                }
                match frame.message {
                    Control::Heartbeat => (),
                    Control::Goodbye => bail!("Server disconnected"),
                    Control::Enter {
                        generation,
                        position,
                    } => {
                        receiver.input.release_all();
                        ensure!(
                            generation > receiver.generation,
                            "Invalid control generation"
                        );
                        receiver.generation = generation;
                        receiver.active = true;
                        receiver.motion = 0;
                        cursor.show(false);
                        receiver.position(position)?;
                        state.ownership(true);
                    }
                    Control::Leave { generation } if generation == receiver.generation => {
                        receiver.input.release_all();
                        receiver.active = false;
                        receiver.pending = None;
                        cursor.hide();
                        state.ownership(false);
                    }
                    Control::Input {
                        generation,
                        position,
                        event,
                    } if receiver.active && generation == receiver.generation => {
                        receiver.position(position)?;
                        match event {
                            MouseEvent::ButtonDown(b) => receiver.input.button(b, true)?,
                            MouseEvent::ButtonUp(b) => receiver.input.button(b, false)?,
                            MouseEvent::Scroll { dx, dy } => {
                                receiver.input.simulator.scroll(dx, dy)?
                            }
                            MouseEvent::Move { .. } => bail!("Movement must use UDP"),
                        }
                    }
                    Control::Key {
                        generation,
                        position,
                        event,
                    } if receiver.active && generation == receiver.generation => {
                        receiver.position(position)?;
                        if features.keyboard && available && desired.keyboard {
                            receiver.input.key(&event)?;
                        }
                    }
                    Control::Settings {
                        features: new,
                        clipboard_epoch,
                    } => {
                        if !new.keyboard {
                            receiver.input.release_keys();
                        }
                        if new.clipboard != features.clipboard || clipboard_epoch != epoch {
                            clipboard.take();
                            epoch = clipboard_epoch;
                            if new.clipboard && desired.clipboard {
                                clipboard =
                                    Some(clip(clip_addr, false, id, epoch, state, backend)?);
                            }
                        }
                        features = new;
                        state.features(features, available, edge);
                        queue(
                            &mut wire,
                            id,
                            0,
                            Control::SettingsAck {
                                features,
                                clipboard_epoch: epoch,
                            },
                        )?;
                    }
                    _ => bail!("Unexpected control message"),
                }
                if frame.sequence > 0 {
                    receiver.control = frame.sequence;
                    receiver.apply_pending()?;
                    queue(&mut wire, id, 0, Control::Applied(frame.sequence))?;
                }
                rx = Instant::now();
                state
                    .last_heartbeat_ms
                    .store(super::state::now_ms(), Ordering::SeqCst);
            }
            if clipboard.as_ref().is_some_and(|c| c.is_finished()) {
                clipboard.take();
                queue(
                    &mut wire,
                    id,
                    0,
                    Control::ClipboardFault(
                        state
                            .snapshot()
                            .clipboard_error
                            .unwrap_or_else(|| "Clipboard worker stopped".into()),
                    ),
                )?;
            }
            ensure!(rx.elapsed() < TIMEOUT, "Server heartbeat timeout");
            if hb.elapsed() >= Duration::from_secs(1) {
                queue(&mut wire, id, 0, Control::Heartbeat)?;
                hb = Instant::now();
            }
            let mut buf = [0; 4096];
            for _ in 0..256 {
                match udp.recv_from(&mut buf) {
                    Ok((n, source)) if source == addr => {
                        if let Ok(packet) = bincode::deserialize::<Motion>(&buf[..n]) {
                            if packet.magic == protocol::MAGIC && packet.session == id {
                                if latest_move.as_ref().is_none_or(|old| {
                                    (packet.generation, packet.position.motion)
                                        > (old.generation, old.position.motion)
                                }) {
                                    latest_move = Some(packet);
                                }
                            }
                        }
                    }
                    Ok(_) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
            if Instant::now() >= next_move {
                if let Some(packet) = latest_move.take() {
                    receiver.motion(packet)?;
                }
                next_move = Instant::now() + frame_interval;
            }
            pause();
        }
        Ok(())
    })();
    receiver.input.release_all();
    cursor.show(true);
    state.ownership(false);
    finish_wire(&mut wire, id);
    drop(clipboard);
    result
}
