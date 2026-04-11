use crate::input::{capture, simulate};
use crate::net::state::{now_ms, SharedState};
use crate::protocol::{self, Message, MouseEventType};
use crate::screen::get_screen_info;
use anyhow::Result;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Client {
    server_addr: String,
}

impl Client {
    pub fn new(server_addr: String) -> Self {
        Self { server_addr }
    }

    pub fn run(&self, state: Arc<SharedState>) -> Result<()> {
        state.started_ms.store(now_ms(), Ordering::SeqCst);
        state.clear_error();
        state.set_peer(self.server_addr.clone());

        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        log::info!("Connecting to server at {}", self.server_addr);

        // Get local screen info
        let screen = get_screen_info()?;
        log::info!("Client screen: {}x{}", screen.width, screen.height);

        // Send Hello with retries
        let hello = protocol::serialize(&Message::Hello(screen.clone()))?;
        let server_screen = match self.connect_with_retry(&socket, &hello, &state) {
            Ok(s) => s,
            Err(e) => {
                state.set_error(format!("{}", e));
                return Err(e);
            }
        };
        if state.shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        log::info!(
            "Connected to server (screen: {}x{})",
            server_screen.width,
            server_screen.height
        );
        state.connected.store(true, Ordering::SeqCst);
        state.last_heartbeat_ms.store(now_ms(), Ordering::SeqCst);

        // Start clipboard TCP client in background (port = udp_port + 1)
        let server_sock: SocketAddr = self.server_addr.parse()?;
        let clipboard_addr = SocketAddr::new(server_sock.ip(), server_sock.port() + 1);
        let clip_shutdown = Arc::new(AtomicBool::new(false));
        let clip_shutdown_for_thread = clip_shutdown.clone();
        let clipboard_thread = std::thread::Builder::new()
            .name("clipboard-client".into())
            .spawn(move || {
                crate::clipboard::run_client(clipboard_addr, clip_shutdown_for_thread);
            })?;

        // Create simulator
        let mut simulator = simulate::create_simulator();

        // Mouse is on the server by default — hide our local cursor so the
        // user sees only the remote cursor. Explicitly tracked to guarantee
        // hide/show stay balanced across Enter/Leave transitions (duplicate
        // messages or unexpected ordering won't drift the refcount).
        capture::hide_local_cursor();
        let mut cursor_hidden = true;

        // Event loop
        let mut buf = [0u8; 4096];
        let mut active = false;
        let mut last_heartbeat = Instant::now();
        let mut last_move_log = Instant::now();
        let mut sim_x: f64 = 0.0;
        let mut sim_y: f64 = 0.0;

        let loop_result: Result<()> = 'evt: loop {
            // Graceful shutdown request from UI
            if state.shutdown.load(Ordering::SeqCst) {
                log::info!("Client event loop: shutdown requested");
                break 'evt Ok(());
            }

            // Send heartbeat
            if last_heartbeat.elapsed() > Duration::from_secs(1) {
                let hb = protocol::serialize(&Message::Heartbeat)?;
                let _ = socket.send_to(&hb, &self.server_addr);
                last_heartbeat = Instant::now();
            }

            match socket.recv_from(&mut buf) {
                Ok((len, _)) => {
                    let msg = match protocol::deserialize(&buf[..len]) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };

                    match msg {
                        Message::Enter { x, y } => {
                            active = true;
                            state.mouse_on_peer.store(true, Ordering::SeqCst);
                            sim_x = x;
                            sim_y = y;
                            // Show cursor on entry so the user sees it.
                            if cursor_hidden {
                                capture::show_local_cursor();
                                cursor_hidden = false;
                            }
                            log::info!("Mouse entered at ({:.0}, {:.0})", x, y);
                            if let Err(e) = simulator.move_to(x, y) {
                                log::error!("Failed to move cursor: {}", e);
                            }
                        }
                        Message::Leave => {
                            active = false;
                            state.mouse_on_peer.store(false, Ordering::SeqCst);
                            // Mouse is going back to the server — hide local cursor.
                            if !cursor_hidden {
                                capture::hide_local_cursor();
                                cursor_hidden = true;
                            }
                            log::info!("Mouse left client screen");
                        }
                        Message::KeyInput(key) if active => {
                            log::info!(
                                "received key: code={} down={} flags=0x{:x}",
                                key.keycode, key.down, key.flags
                            );
                            if let Err(e) = simulator.key_event(key.keycode, key.down, key.flags) {
                                log::error!("Key simulation error: {}", e);
                            }
                            state.events_total.fetch_add(1, Ordering::Relaxed);
                            state.last_event_ms.store(now_ms(), Ordering::SeqCst);
                        }
                        Message::Input(event) if active => {
                            state.events_total.fetch_add(1, Ordering::Relaxed);
                            state.last_event_ms.store(now_ms(), Ordering::SeqCst);
                            let result = match &event.event_type {
                                MouseEventType::Move => {
                                    sim_x += event.dx;
                                    sim_y += event.dy;
                                    if last_move_log.elapsed() > Duration::from_millis(1000) {
                                        log::info!(
                                            "sim cursor=({:.0},{:.0}) dx={:.1} dy={:.1}",
                                            sim_x, sim_y, event.dx, event.dy
                                        );
                                        last_move_log = Instant::now();
                                    }
                                    simulator.move_relative(event.dx, event.dy)
                                }
                                MouseEventType::ButtonDown(btn) => simulator.button_down(*btn),
                                MouseEventType::ButtonUp(btn) => simulator.button_up(*btn),
                                MouseEventType::Scroll { dx, dy } => {
                                    simulator.scroll(*dx, *dy)
                                }
                            };
                            if let Err(e) = result {
                                log::error!("Simulation error: {}", e);
                            }
                        }
                        Message::Heartbeat => {
                            state.last_heartbeat_ms.store(now_ms(), Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    log::error!("Network error: {}", e);
                    state.set_error(format!("{}", e));
                }
            }
        };

        // Graceful teardown.
        state.connected.store(false, Ordering::SeqCst);
        state.mouse_on_peer.store(false, Ordering::SeqCst);
        if cursor_hidden {
            capture::show_local_cursor();
        }
        clip_shutdown.store(true, Ordering::SeqCst);
        let _ = clipboard_thread.join();
        loop_result
    }

    fn connect_with_retry(
        &self,
        socket: &UdpSocket,
        hello: &[u8],
        state: &Arc<SharedState>,
    ) -> Result<crate::protocol::ScreenInfo> {
        let mut buf = [0u8; 4096];
        for attempt in 0..10 {
            if state.shutdown.load(Ordering::SeqCst) {
                return Err(anyhow::anyhow!("connect cancelled"));
            }
            if attempt > 0 {
                log::info!("Retrying Hello (attempt {}/10)...", attempt + 1);
            }
            let _ = socket.send_to(hello, &self.server_addr);

            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if state.shutdown.load(Ordering::SeqCst) {
                    return Err(anyhow::anyhow!("connect cancelled"));
                }
                match socket.recv_from(&mut buf) {
                    Ok((len, _)) => {
                        if let Ok(Message::HelloAck(screen)) =
                            protocol::deserialize(&buf[..len])
                        {
                            return Ok(screen);
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Err(anyhow::anyhow!(
            "Failed to connect to server after 10 attempts"
        ))
    }
}
