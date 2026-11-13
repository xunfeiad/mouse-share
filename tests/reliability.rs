use anyhow::Result;
use mouse_share::{
    clipboard::ClipboardBackend,
    config::Edge,
    input::{
        capture::{CaptureControl, CapturedInput, InputCapture, InputRouting},
        pressed::PressedInputState,
        simulate::InputSimulator,
    },
    net::{
        runtime::Backend,
        session::{Role, SessionCommand, SessionConfig, SessionHandle},
        state::Phase,
        transport::Framed,
    },
    protocol::{
        self, Control, ControlFrame, Features, KeyEvent, Motion, MouseButton, MouseEvent, Position,
        ScreenInfo,
    },
};
use std::{
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq)]
enum Op {
    Position(f64, f64),
    Button(MouseButton, bool),
    Key(u32, bool),
    Cursor(bool),
    Clipboard(String),
}
#[derive(Default)]
struct Fake {
    control: Arc<CaptureControl>,
    sender: Arc<Mutex<Option<crossbeam_channel::Sender<CapturedInput>>>>,
    operations: Arc<Mutex<Vec<Op>>>,
    text: Arc<Mutex<String>>,
    fail_up: Arc<AtomicBool>,
    initially_held: AtomicBool,
}
struct Capture {
    control: Arc<CaptureControl>,
    sender: Arc<Mutex<Option<crossbeam_channel::Sender<CapturedInput>>>>,
}
impl InputCapture for Capture {
    fn run(
        &mut self,
        tx: crossbeam_channel::Sender<CapturedInput>,
        stop: Arc<AtomicBool>,
    ) -> Result<()> {
        *self.sender.lock().unwrap() = Some(tx);
        self.control.ready.store(true, Ordering::SeqCst);
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
    fn control_handle(&self) -> Arc<CaptureControl> {
        self.control.clone()
    }
}
struct Simulator {
    operations: Arc<Mutex<Vec<Op>>>,
    fail_up: Arc<AtomicBool>,
}
impl InputSimulator for Simulator {
    fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        self.operations.lock().unwrap().push(Op::Position(x, y));
        Ok(())
    }
    fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.move_to(dx, dy)
    }
    fn button_down(&mut self, b: MouseButton) -> Result<()> {
        self.operations.lock().unwrap().push(Op::Button(b, true));
        Ok(())
    }
    fn button_up(&mut self, b: MouseButton) -> Result<()> {
        self.operations.lock().unwrap().push(Op::Button(b, false));
        Ok(())
    }
    fn key_event(&mut self, k: u32, down: bool, _: u64) -> Result<()> {
        if !down && self.fail_up.swap(false, Ordering::SeqCst) {
            anyhow::bail!("injected release failure");
        }
        self.operations.lock().unwrap().push(Op::Key(k, down));
        Ok(())
    }
    fn scroll(&mut self, _: f64, _: f64) -> Result<()> {
        Ok(())
    }
}
struct Clipboard {
    operations: Arc<Mutex<Vec<Op>>>,
    text: Arc<Mutex<String>>,
}
impl ClipboardBackend for Clipboard {
    fn get(&mut self) -> Result<Option<String>> {
        Ok(Some(self.text.lock().unwrap().clone()))
    }
    fn set(&mut self, text: String) -> Result<()> {
        *self.text.lock().unwrap() = text.clone();
        self.operations.lock().unwrap().push(Op::Clipboard(text));
        Ok(())
    }
}
impl Backend for Fake {
    fn inputs_held(&self) -> bool {
        self.initially_held.load(Ordering::SeqCst)
    }
    fn screen(&self) -> Result<ScreenInfo> {
        Ok(ScreenInfo {
            width: 1920,
            height: 1080,
        })
    }
    fn capture(&self) -> Box<dyn InputCapture> {
        Box::new(Capture {
            control: self.control.clone(),
            sender: self.sender.clone(),
        })
    }
    fn simulator(&self) -> Box<dyn InputSimulator> {
        Box::new(Simulator {
            operations: self.operations.clone(),
            fail_up: self.fail_up.clone(),
        })
    }
    fn cursor(&self, hidden: bool, _: bool) {
        self.operations.lock().unwrap().push(Op::Cursor(hidden));
    }
    fn clipboard(&self) -> Result<Box<dyn ClipboardBackend>> {
        Ok(Box::new(Clipboard {
            operations: self.operations.clone(),
            text: self.text.clone(),
        }))
    }
}
impl Fake {
    fn emit(&self, event: MouseEvent, x: f64) {
        self.sender
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(CapturedInput::Mouse {
                event,
                abs_x: x,
                abs_y: 300.,
                remote: self.control.mouse.load(Ordering::SeqCst),
            })
            .unwrap();
    }
    fn key(&self, code: u32, down: bool) {
        self.sender
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(CapturedInput::Key {
                event: KeyEvent {
                    keycode: code,
                    down,
                    flags: 0,
                },
                remote: self.control.keyboard.load(Ordering::SeqCst),
            })
            .unwrap();
    }
    fn has(&self, op: Op) -> bool {
        self.operations.lock().unwrap().contains(&op)
    }
}
fn until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < timeout, "condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    until(Duration::from_secs(3), &mut condition)
}
fn port() -> u16 {
    loop {
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = tcp.local_addr().unwrap().port();
        if p < 65535
            && UdpSocket::bind(("127.0.0.1", p)).is_ok()
            && TcpListener::bind(("127.0.0.1", p + 1)).is_ok()
        {
            return p;
        }
    }
}
fn config(role: Role, p: u16) -> SessionConfig {
    SessionConfig {
        role,
        port: p,
        server: format!("127.0.0.1:{p}"),
        edge: Edge::Right,
        device: protocol::new_id(),
        features: Features {
            keyboard: true,
            clipboard: false,
        },
        auto_reconnect: false,
    }
}
fn stop(s: &mut SessionHandle) {
    let start = Instant::now();
    s.command(SessionCommand::Stop);
    assert!(start.elapsed() < Duration::from_millis(100));
    until(Duration::from_secs(1), || s.is_finished());
    s.reap();
    assert_eq!(s.snapshot().phase, Phase::Idle);
}
fn pair() -> (SessionHandle, SessionHandle, Arc<Fake>, Arc<Fake>) {
    let p = port();
    let a = Arc::new(Fake::default());
    let b = Arc::new(Fake::default());
    let server = SessionHandle::start_with_backend(config(Role::Server, p), a.clone()).unwrap();
    wait(|| a.control.ready.load(Ordering::SeqCst));
    let mut client_config = config(Role::Client, p);
    client_config.features.clipboard = true;
    let client = SessionHandle::start_with_backend(client_config, b.clone()).unwrap();
    wait(|| {
        server.snapshot().phase == Phase::Connected && client.snapshot().phase == Phase::Connected
    });
    (server, client, a, b)
}
fn enter(server: &SessionHandle, client: &SessionHandle, a: &Fake) {
    a.emit(MouseEvent::Move { dx: 1., dy: 0. }, 1919.);
    wait(|| server.snapshot().mouse_on_peer && client.snapshot().mouse_on_peer);
    a.emit(MouseEvent::Move { dx: 40., dy: 0. }, 1.);
}

#[test]
fn quiet_connected_session_keeps_control_and_stop_releases_pressed_input() {
    let (mut server, mut client, a, b) = pair();
    enter(&server, &client, &a);
    a.key(56, true);
    a.emit(MouseEvent::ButtonDown(MouseButton::Left), 1.);
    wait(|| b.has(Op::Key(56, true)) && b.has(Op::Button(MouseButton::Left, true)));
    std::thread::sleep(Duration::from_millis(5400));
    assert!(
        server.snapshot().mouse_on_peer && client.snapshot().mouse_on_peer,
        "Idle is not a disconnect"
    );
    stop(&mut client);
    wait(|| !server.snapshot().mouse_on_peer);
    assert!(b.has(Op::Key(56, false)) && b.has(Op::Button(MouseButton::Left, false)));
    assert!(!a.control.mouse.load(Ordering::SeqCst));
    assert!(b.has(Op::Cursor(false)));
    stop(&mut server);
}
#[test]
fn keyboard_setting_releases_held_key_and_preserves_mouse() {
    let (mut server, mut client, a, b) = pair();
    enter(&server, &client, &a);
    a.key(56, true);
    wait(|| b.has(Op::Key(56, true)));
    server.command(SessionCommand::SetFeatures(Features {
        keyboard: false,
        clipboard: false,
    }));
    wait(|| !server.snapshot().features.keyboard && !a.control.keyboard.load(Ordering::SeqCst));
    assert!(b.has(Op::Key(56, false)));
    assert!(a.control.mouse.load(Ordering::SeqCst));
    a.key(12, true);
    std::thread::sleep(Duration::from_millis(50));
    assert!(!b.has(Op::Key(12, true)));
    stop(&mut client);
    stop(&mut server);
}
#[test]
fn reconnect_new_udp_endpoint_replaces_old_session() {
    let p = port();
    let a = Arc::new(Fake::default());
    let b = Arc::new(Fake::default());
    let mut server = SessionHandle::start_with_backend(config(Role::Server, p), a.clone()).unwrap();
    wait(|| a.control.ready.load(Ordering::SeqCst));
    let c = config(Role::Client, p);
    let mut first = SessionHandle::start_with_backend(c.clone(), b.clone()).unwrap();
    wait(|| server.snapshot().phase == Phase::Connected);
    enter(&server, &first, &a);
    a.key(20, true);
    wait(|| b.has(Op::Key(20, true)));
    let old = server.snapshot().peer.unwrap();
    let d = Arc::new(Fake::default());
    let mut second = SessionHandle::start_with_backend(c, d.clone()).unwrap();
    wait(|| {
        second.snapshot().phase == Phase::Connected && server.snapshot().peer.as_ref() != Some(&old)
    });
    wait(|| first.is_finished());
    first.reap();
    assert!(b.has(Op::Key(20, false)));
    a.key(20, false);
    a.emit(MouseEvent::Move { dx: 1., dy: 0. }, 1919.);
    wait(|| second.snapshot().mouse_on_peer);
    a.emit(MouseEvent::Move { dx: 80., dy: 0. }, 1.);
    wait(|| d.has(Op::Position(80., 300.)));
    assert!(!b.has(Op::Position(80., 300.)));
    stop(&mut second);
    stop(&mut server);
}
#[test]
fn routing_consumes_remote_tails_without_orphan_local_events() {
    let mut routing = InputRouting::default();
    assert_eq!(routing.key(12, true, true), (true, true));
    assert_eq!(routing.key(12, true, false), (true, false));
    assert_eq!(routing.key(12, false, false), (true, false));
    assert_eq!(routing.key(12, true, false), (false, false));
    assert_eq!(routing.key(12, false, true), (false, false));
    assert_eq!(
        routing.mouse(&MouseEvent::ButtonDown(MouseButton::Left), true),
        (true, true)
    );
    assert_eq!(
        routing.mouse(&MouseEvent::ButtonUp(MouseButton::Left), false),
        (true, false)
    );
}
#[test]
fn release_is_idempotent_and_retries_failed_keys_without_skipping_buttons() {
    let backend = Fake::default();
    let mut pressed = PressedInputState::new(backend.simulator());
    pressed
        .key(&KeyEvent {
            keycode: 56,
            down: true,
            flags: 0,
        })
        .unwrap();
    pressed.button(MouseButton::Left, true).unwrap();
    backend.fail_up.store(true, Ordering::SeqCst);
    pressed.release_all();
    assert!(backend.has(Op::Button(MouseButton::Left, false)));
    pressed.release_all();
    pressed.release_all();
    drop(pressed);
    assert_eq!(
        backend
            .operations
            .lock()
            .unwrap()
            .iter()
            .filter(|o| **o == Op::Key(56, false))
            .count(),
        1
    );
    assert_eq!(
        backend
            .operations
            .lock()
            .unwrap()
            .iter()
            .filter(|o| **o == Op::Button(MouseButton::Left, false))
            .count(),
        1
    );
}
#[test]
fn capture_fault_releases_local_control_and_remote_keys() {
    let (mut server, mut client, a, b) = pair();
    enter(&server, &client, &a);
    a.key(56, true);
    wait(|| b.has(Op::Key(56, true)));
    a.control.fault.store(true, Ordering::SeqCst);
    wait(|| server.is_finished() && client.is_finished());
    server.reap();
    client.reap();
    assert!(!a.control.mouse.load(Ordering::SeqCst));
    assert!(b.has(Op::Key(56, false)));
    assert!(b.has(Op::Cursor(false)));
}
#[test]
fn held_local_input_prevents_entering_remote() {
    let (mut server, mut client, a, _) = pair();
    a.initially_held.store(true, Ordering::SeqCst);
    a.emit(MouseEvent::Move { dx: 1., dy: 0. }, 1919.);
    std::thread::sleep(Duration::from_millis(50));
    assert!(!server.snapshot().mouse_on_peer);
    a.initially_held.store(false, Ordering::SeqCst);
    a.key(12, true);
    a.emit(MouseEvent::Move { dx: 1., dy: 0. }, 1919.);
    std::thread::sleep(Duration::from_millis(50));
    assert!(!server.snapshot().mouse_on_peer);
    a.key(12, false);
    enter(&server, &client, &a);
    stop(&mut client);
    stop(&mut server);
}
#[test]
fn stop_and_disable_auto_reconnect_cancel_backoff() {
    let p = port();
    let mut c = config(Role::Client, p);
    c.auto_reconnect = true;
    let mut client =
        SessionHandle::start_with_backend(c.clone(), Arc::new(Fake::default())).unwrap();
    wait(|| client.snapshot().phase == Phase::Reconnecting);
    stop(&mut client);
    let mut client = SessionHandle::start_with_backend(c, Arc::new(Fake::default())).unwrap();
    wait(|| client.snapshot().phase == Phase::Reconnecting);
    client.command(SessionCommand::SetAutoReconnect(false));
    until(Duration::from_secs(1), || client.is_finished());
    client.reap();
    assert_eq!(client.snapshot().phase, Phase::Failed);
}
#[test]
fn framed_partial_messages_and_half_close_are_detected() {
    use std::io::Write;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut tx = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (rx, _) = listener.accept().unwrap();
    rx.set_nonblocking(true).unwrap();
    let mut wire = Framed::new(rx, 1024).unwrap();
    let bytes = bincode::serialize(&"test".to_string()).unwrap();
    let len = (bytes.len() as u32).to_be_bytes();
    tx.write_all(&len[..2]).unwrap();
    assert!(wire.recv::<String>().unwrap().is_none());
    tx.write_all(&len[2..]).unwrap();
    tx.write_all(&bytes[..2]).unwrap();
    assert!(wire.recv::<String>().unwrap().is_none());
    tx.write_all(&bytes[2..]).unwrap();
    let mut value = None;
    wait(|| {
        value = wire.recv::<String>().unwrap();
        value.is_some()
    });
    assert_eq!(value.as_deref(), Some("test"));
    tx.shutdown(std::net::Shutdown::Write).unwrap();
    wait(|| wire.recv::<String>().is_err());
}
fn raw_frame(wire: &mut Framed, id: [u8; 16], sequence: u64, message: Control) {
    wire.queue(&ControlFrame::new(id, sequence, message))
        .unwrap();
    wire.flush().unwrap();
}
fn raw_recv(wire: &mut Framed) -> ControlFrame {
    let mut result = None;
    wait(|| {
        result = wire.recv().unwrap();
        result.is_some()
    });
    result.unwrap()
}
#[test]
fn cross_channel_order_ignores_old_sessions_and_old_positions() {
    let p = port();
    let listener = TcpListener::bind(("127.0.0.1", p)).unwrap();
    let udp = UdpSocket::bind(("127.0.0.1", p)).unwrap();
    let b = Arc::new(Fake::default());
    let mut client = SessionHandle::start_with_backend(config(Role::Client, p), b.clone()).unwrap();
    let (stream, addr) = listener.accept().unwrap();
    let mut wire = Framed::new(stream, 1024 * 1024).unwrap();
    let hello = raw_recv(&mut wire);
    let Control::Hello { udp_port, .. } = hello.message else {
        panic!("expected hello")
    };
    let target = SocketAddr::new(addr.ip(), udp_port);
    let id = protocol::new_id();
    raw_frame(
        &mut wire,
        id,
        0,
        Control::Welcome {
            version: 2,
            session: id,
            platform: protocol::platform(),
            screen: ScreenInfo {
                width: 1920,
                height: 1080,
            },
            edge: Edge::Right,
            features: Features {
                keyboard: true,
                clipboard: false,
            },
            clipboard_epoch: 1,
        },
    );
    raw_recv(&mut wire);
    raw_frame(
        &mut wire,
        id,
        1,
        Control::Enter {
            generation: 1,
            position: Position {
                x: 0.,
                y: 300.,
                motion: 0,
            },
        },
    );
    wait(|| client.snapshot().mouse_on_peer);
    // Delayed frames from a replaced session must not stop or reconfigure it.
    raw_frame(&mut wire, protocol::new_id(), 0, Control::Goodbye);
    raw_frame(
        &mut wire,
        protocol::new_id(),
        2,
        Control::Settings {
            features: Features {
                keyboard: false,
                clipboard: true,
            },
            clipboard_epoch: 99,
        },
    );
    let motion = |session, after_control, seq, x| Motion {
        magic: protocol::MAGIC,
        session,
        generation: 1,
        after_control,
        position: Position {
            x,
            y: 300.,
            motion: seq,
        },
    };
    udp.send_to(
        &bincode::serialize(&motion(protocol::new_id(), 1, 10, 999.)).unwrap(),
        target,
    )
    .unwrap();
    udp.send_to(
        &bincode::serialize(&motion(id, 2, 3, 300.)).unwrap(),
        target,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert!(!b.has(Op::Position(300., 300.)) && !b.has(Op::Position(999., 300.)));
    raw_frame(
        &mut wire,
        id,
        2,
        Control::Input {
            generation: 1,
            position: Position {
                x: 200.,
                y: 300.,
                motion: 2,
            },
            event: MouseEvent::ButtonDown(MouseButton::Left),
        },
    );
    wait(|| b.has(Op::Position(300., 300.)));
    let events = b.operations.lock().unwrap().clone();
    let click = events
        .iter()
        .position(|o| *o == Op::Button(MouseButton::Left, true))
        .unwrap();
    assert_eq!(events[click - 1], Op::Position(200., 300.));
    assert_eq!(events[click + 1], Op::Position(300., 300.));
    udp.send_to(
        &bincode::serialize(&motion(id, 1, 1, 100.)).unwrap(),
        target,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert!(!b.has(Op::Position(100., 300.)));
    assert!(client.snapshot().features.keyboard);
    raw_frame(
        &mut wire,
        id,
        3,
        Control::Key {
            generation: 1,
            position: Position {
                x: 300.,
                y: 300.,
                motion: 3,
            },
            event: KeyEvent {
                keycode: 56,
                down: true,
                flags: 0,
            },
        },
    );
    wait(|| b.has(Op::Key(56, true)));
    // UDP traffic must not keep a dead reliable control connection alive.
    let timeout = Instant::now();
    let mut sequence = 4;
    while !client.is_finished() {
        udp.send_to(
            &bincode::serialize(&motion(id, 3, sequence, 350.)).unwrap(),
            target,
        )
        .unwrap();
        sequence += 1;
        assert!(timeout.elapsed() < Duration::from_millis(5500));
        std::thread::sleep(Duration::from_millis(10));
    }
    client.reap();
    assert!(timeout.elapsed() >= Duration::from_secs(4));
    assert!(b.has(Op::Button(MouseButton::Left, false)));
    assert!(b.has(Op::Key(56, false)) && b.has(Op::Cursor(false)));
}

#[test]
fn leaving_remote_releases_buttons_and_modifiers_before_next_entry() {
    let (mut server, mut client, a, b) = pair();
    enter(&server, &client, &a);
    a.key(56, true);
    a.emit(MouseEvent::ButtonDown(MouseButton::Left), 1.);
    wait(|| b.has(Op::Key(56, true)) && b.has(Op::Button(MouseButton::Left, true)));
    a.emit(MouseEvent::Move { dx: -80., dy: 0. }, 1.);
    wait(|| !client.snapshot().mouse_on_peer && !server.snapshot().mouse_on_peer);
    assert!(b.has(Op::Key(56, false)) && b.has(Op::Button(MouseButton::Left, false)));
    a.emit(MouseEvent::Move { dx: 1., dy: 0. }, 1919.);
    std::thread::sleep(Duration::from_millis(30));
    assert!(
        !server.snapshot().mouse_on_peer,
        "held remote tails cannot enter a new generation"
    );
    a.key(56, false);
    a.emit(MouseEvent::ButtonUp(MouseButton::Left), 1.);
    enter(&server, &client, &a);
    stop(&mut client);
    stop(&mut server);
    assert_eq!(
        b.operations
            .lock()
            .unwrap()
            .iter()
            .filter(|op| **op == Op::Key(56, false))
            .count(),
        1
    );
}
#[test]
fn server_heartbeat_blackhole_releases_even_with_continuous_input() {
    let p = port();
    let a = Arc::new(Fake::default());
    let mut server = SessionHandle::start_with_backend(config(Role::Server, p), a.clone()).unwrap();
    wait(|| a.control.ready.load(Ordering::SeqCst));
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let stream = TcpStream::connect(("127.0.0.1", p)).unwrap();
    let mut wire = Framed::new(stream, 1024 * 1024).unwrap();
    raw_frame(
        &mut wire,
        [0; 16],
        0,
        Control::Hello {
            version: 2,
            device: protocol::new_id(),
            platform: protocol::platform(),
            screen: ScreenInfo {
                width: 1920,
                height: 1080,
            },
            udp_port: udp.local_addr().unwrap().port(),
            features: Features {
                keyboard: true,
                clipboard: false,
            },
        },
    );
    let frame = raw_recv(&mut wire);
    let id = frame.session;
    raw_frame(&mut wire, id, 0, Control::Ready);
    wait(|| server.snapshot().phase == Phase::Connected);
    a.emit(MouseEvent::Move { dx: 1., dy: 0. }, 1919.);
    wait(|| server.snapshot().mouse_on_peer);
    let enter_frame = raw_recv(&mut wire);
    raw_frame(&mut wire, id, 0, Control::Applied(enter_frame.sequence));
    let start = Instant::now();
    while server.snapshot().mouse_on_peer {
        a.emit(MouseEvent::Move { dx: 0., dy: 0. }, 1.);
        assert!(start.elapsed() < Duration::from_millis(5500));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(start.elapsed() >= Duration::from_secs(4));
    assert!(!a.control.mouse.load(Ordering::SeqCst));
    stop(&mut server);
}

#[test]
fn clipboard_toggle_controls_both_directions_without_stopping_input() {
    let (mut server, mut client, a, b) = pair();
    server.command(SessionCommand::SetFeatures(Features {
        keyboard: true,
        clipboard: true,
    }));
    wait(|| server.snapshot().features.clipboard && client.snapshot().features.clipboard);
    *a.text.lock().unwrap() = "alpha".into();
    wait(|| *b.text.lock().unwrap() == "alpha");
    server.command(SessionCommand::SetFeatures(Features {
        keyboard: true,
        clipboard: false,
    }));
    wait(|| !server.snapshot().features.clipboard && !client.snapshot().features.clipboard);
    *a.text.lock().unwrap() = "private-a".into();
    *b.text.lock().unwrap() = "private-b".into();
    std::thread::sleep(Duration::from_millis(650));
    assert_eq!(*a.text.lock().unwrap(), "private-a");
    assert_eq!(*b.text.lock().unwrap(), "private-b");
    enter(&server, &client, &a);
    a.key(12, true);
    wait(|| b.has(Op::Key(12, true)));
    a.key(12, false);
    wait(|| b.has(Op::Key(12, false)));
    stop(&mut client);
    stop(&mut server);
}
#[test]
fn clipboard_partial_frame_cancel_and_old_epoch_do_not_apply() {
    use mouse_share::{clipboard::ClipboardWorker, net::state::SharedState};
    use std::io::Write;
    #[derive(serde::Serialize, serde::Deserialize)]
    enum Packet {
        Bind {
            magic: [u8; 4],
            session: [u8; 16],
            epoch: u64,
        },
        Ready,
        Text(String),
    }
    let p = port();
    let addr: SocketAddr = format!("127.0.0.1:{p}").parse().unwrap();
    let id = protocol::new_id();
    let b = Arc::new(Fake::default());
    let factory = b.clone();
    let state = Arc::new(SharedState::new());
    let worker = ClipboardWorker::start(
        addr,
        true,
        id,
        1,
        state.clone(),
        Arc::new(move || factory.clipboard()),
    )
    .unwrap();
    let stream = TcpStream::connect(addr).unwrap();
    let mut wire = Framed::new(stream, 64 * 1024 * 1024).unwrap();
    wire.queue(&Packet::Bind {
        magic: protocol::MAGIC,
        session: id,
        epoch: 1,
    })
    .unwrap();
    wire.flush().unwrap();
    wait(|| matches!(wire.recv::<Packet>().unwrap(), Some(Packet::Ready)));
    let payload = bincode::serialize(&Packet::Text("old".repeat(1000))).unwrap();
    wire.stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    wire.stream.write_all(&payload[..8]).unwrap();
    let start = Instant::now();
    drop(worker);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(!b.has(Op::Clipboard("old".repeat(1000))));
    let factory = b.clone();
    let worker = ClipboardWorker::start(
        addr,
        true,
        id,
        2,
        state,
        Arc::new(move || factory.clipboard()),
    )
    .unwrap();
    let mut old = Framed::new(TcpStream::connect(addr).unwrap(), 1024).unwrap();
    old.queue(&Packet::Bind {
        magic: protocol::MAGIC,
        session: id,
        epoch: 1,
    })
    .unwrap();
    old.flush().unwrap();
    wait(|| old.recv::<Packet>().is_err());
    let mut current = Framed::new(TcpStream::connect(addr).unwrap(), 1024).unwrap();
    current
        .queue(&Packet::Bind {
            magic: protocol::MAGIC,
            session: id,
            epoch: 2,
        })
        .unwrap();
    current.flush().unwrap();
    wait(|| matches!(current.recv::<Packet>().unwrap(), Some(Packet::Ready)));
    current.queue(&Packet::Text("new".into())).unwrap();
    current.flush().unwrap();
    wait(|| b.has(Op::Clipboard("new".into())));
    current.stream.shutdown(std::net::Shutdown::Write).unwrap();
    wait(|| current.recv::<Packet>().is_err());
    let start = Instant::now();
    drop(worker);
    assert!(start.elapsed() < Duration::from_secs(1));
}
#[test]
fn large_clipboard_does_not_block_button_release() {
    let (mut server, mut client, a, b) = pair();
    server.command(SessionCommand::SetFeatures(Features {
        keyboard: true,
        clipboard: true,
    }));
    wait(|| server.snapshot().features.clipboard && client.snapshot().features.clipboard);
    enter(&server, &client, &a);
    *a.text.lock().unwrap() = "x".repeat(4 * 1024 * 1024);
    a.emit(MouseEvent::ButtonDown(MouseButton::Left), 1.);
    a.emit(MouseEvent::ButtonUp(MouseButton::Left), 1.);
    until(Duration::from_secs(1), || {
        b.has(Op::Button(MouseButton::Left, false))
    });
    stop(&mut client);
    stop(&mut server);
}
#[test]
fn capture_queue_full_fails_open_instead_of_losing_key_release_silently() {
    let control = CaptureControl::default();
    control.mouse.store(true, Ordering::SeqCst);
    control.keyboard.store(true, Ordering::SeqCst);
    let (tx, _rx) = crossbeam_channel::bounded(1);
    let event = CapturedInput::Key {
        event: KeyEvent {
            keycode: 56,
            down: false,
            flags: 0,
        },
        remote: true,
    };
    control.send(&tx, event.clone());
    control.send(&tx, event);
    assert!(control.fault.load(Ordering::SeqCst));
    assert!(!control.mouse.load(Ordering::SeqCst));
    assert!(!control.keyboard.load(Ordering::SeqCst));
}

#[test]
fn client_features_negotiate_and_release_on_runtime_disable() {
    let p = port();
    let a = Arc::new(Fake::default());
    let b = Arc::new(Fake::default());
    let mut server = SessionHandle::start_with_backend(config(Role::Server, p), a.clone()).unwrap();
    wait(|| a.control.ready.load(Ordering::SeqCst));
    let mut c = config(Role::Client, p);
    c.features.keyboard = false;
    let mut client = SessionHandle::start_with_backend(c, b.clone()).unwrap();
    wait(|| server.snapshot().phase == Phase::Connected);
    assert!(!server.snapshot().features.keyboard);
    enter(&server, &client, &a);
    client.command(SessionCommand::SetFeatures(Features {
        keyboard: true,
        clipboard: false,
    }));
    wait(|| server.snapshot().features.keyboard && a.control.keyboard.load(Ordering::SeqCst));
    a.key(56, true);
    wait(|| b.has(Op::Key(56, true)));
    client.command(SessionCommand::SetFeatures(Features {
        keyboard: false,
        clipboard: false,
    }));
    wait(|| b.has(Op::Key(56, false)) && !server.snapshot().features.keyboard);
    assert!(server.snapshot().mouse_on_peer);
    stop(&mut client);
    stop(&mut server);
}

#[test]
fn automatic_reconnect_replaces_session_after_server_restart() {
    let p = port();
    let a = Arc::new(Fake::default());
    let b = Arc::new(Fake::default());
    let mut server = SessionHandle::start_with_backend(config(Role::Server, p), a.clone()).unwrap();
    wait(|| a.control.ready.load(Ordering::SeqCst));
    let mut c = config(Role::Client, p);
    c.auto_reconnect = true;
    let mut client = SessionHandle::start_with_backend(c, b.clone()).unwrap();
    wait(|| server.snapshot().phase == Phase::Connected);
    enter(&server, &client, &a);
    a.key(56, true);
    wait(|| b.has(Op::Key(56, true)));
    stop(&mut server);
    wait(|| client.snapshot().phase == Phase::Reconnecting && b.has(Op::Key(56, false)));
    let d = Arc::new(Fake::default());
    let mut replacement =
        SessionHandle::start_with_backend(config(Role::Server, p), d.clone()).unwrap();
    wait(|| {
        replacement.snapshot().phase == Phase::Connected
            && client.snapshot().phase == Phase::Connected
    });
    enter(&replacement, &client, &d);
    stop(&mut client);
    wait(|| replacement.snapshot().phase == Phase::Listening);
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        replacement.snapshot().phase,
        Phase::Listening,
        "explicit stop cannot retry"
    );
    stop(&mut replacement);
}

#[test]
fn healthy_session_rejects_other_devices_and_old_protocol() {
    let p = port();
    let a = Arc::new(Fake::default());
    let mut server = SessionHandle::start_with_backend(config(Role::Server, p), a.clone()).unwrap();
    wait(|| a.control.ready.load(Ordering::SeqCst));
    let mut client =
        SessionHandle::start_with_backend(config(Role::Client, p), Arc::new(Fake::default()))
            .unwrap();
    wait(|| server.snapshot().phase == Phase::Connected);
    let other =
        SessionHandle::start_with_backend(config(Role::Client, p), Arc::new(Fake::default()))
            .unwrap();
    wait(|| other.is_finished());
    assert!(other.snapshot().error.unwrap().contains("Another device"));
    assert_eq!(client.snapshot().phase, Phase::Connected);
    let mut wire = Framed::new(TcpStream::connect(("127.0.0.1", p)).unwrap(), 1024 * 1024).unwrap();
    raw_frame(
        &mut wire,
        [0; 16],
        0,
        Control::Hello {
            version: 1,
            device: protocol::new_id(),
            platform: protocol::platform(),
            screen: ScreenInfo {
                width: 1920,
                height: 1080,
            },
            udp_port: 12345,
            features: Features::default(),
        },
    );
    assert!(
        matches!(raw_recv(&mut wire).message, Control::Reject(reason) if reason.contains("upgrade both"))
    );
    assert_eq!(server.snapshot().phase, Phase::Connected);
    assert!(!a.control.mouse.load(Ordering::SeqCst));
    stop(&mut client);
    stop(&mut server);
}
