use crate::config::{Edge, ScreenConfig};
use crate::input::capture::{self, CapturedInput};
use crate::protocol::{self, Message, ScreenInfo};
use crate::screen::get_screen_info;
use anyhow::Result;
use crossbeam_channel::{bounded, Receiver};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SUPPRESS_TIMEOUT: Duration = Duration::from_secs(5);
/// Distance the virtual cursor must travel INSIDE the client screen before
/// the "return to server" check is armed. Without this, entry position and
/// return threshold collide at the same edge and the state machine flips
/// Enter→Return on every event.
const RETURN_ARM_DISTANCE: f64 = 20.0;

pub struct Server {
    port: u16,
    edge: Edge,
}

impl Server {
    pub fn new(port: u16, edge: Edge) -> Self {
        Self { port, edge }
    }

    pub fn run(&self) -> Result<()> {
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", self.port))?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        log::info!("Server listening on 0.0.0.0:{}", self.port);

        // Start clipboard TCP server on port+1 in background
        let clipboard_port = self.port + 1;
        std::thread::Builder::new()
            .name("clipboard-server".into())
            .spawn(move || {
                crate::clipboard::run_server(clipboard_port);
            })?;

        // Wait for client Hello
        let (client_addr, client_screen) = self.wait_for_client(&socket)?;
        log::info!("Client connected: {} ({:?})", client_addr, client_screen);

        // Get server screen info
        let server_screen = get_screen_info()?;
        log::info!("Server screen: {}x{}", server_screen.width, server_screen.height);

        // Send HelloAck
        let ack = protocol::serialize(&Message::HelloAck(server_screen.clone()))?;
        socket.send_to(&ack, client_addr)?;

        // Setup screen config
        let mut screen_config = ScreenConfig::new(server_screen, self.edge);
        screen_config.client_screen = Some(client_screen);

        // Start input capture in a separate thread
        let mut capturer = capture::create_capture();
        let suppress = capturer.suppress_handle();
        let (sender, receiver) = bounded::<CapturedInput>(256);

        let _capture_thread = std::thread::Builder::new()
            .name("input-capture".into())
            .spawn(move || {
                if let Err(e) = capturer.run(sender) {
                    log::error!("Capture error: {}", e);
                }
            })?;

        // Switch to non-blocking for event loop
        socket.set_nonblocking(true)?;

        // Main event loop
        self.event_loop(socket, client_addr, receiver, suppress, &screen_config)?;

        Ok(())
    }

    fn wait_for_client(&self, socket: &UdpSocket) -> Result<(SocketAddr, ScreenInfo)> {
        log::info!("Waiting for client to connect...");
        let mut buf = [0u8; 4096];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((len, addr)) => {
                    if let Ok(Message::Hello(screen)) = protocol::deserialize(&buf[..len]) {
                        return Ok((addr, screen));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn event_loop(
        &self,
        socket: UdpSocket,
        client_addr: SocketAddr,
        receiver: Receiver<CapturedInput>,
        suppress: Arc<AtomicBool>,
        config: &ScreenConfig,
    ) -> Result<()> {
        let mut forwarding = false;
        // Explicit tracking of local cursor visibility. Guarantees hide/show
        // stay paired even across weird state transitions (watchdog, disconnect).
        let mut cursor_hidden = false;
        let mut last_heartbeat = Instant::now();
        // Track virtual cursor position on the client screen
        let mut client_cursor_x: f64 = 0.0;
        let mut client_cursor_y: f64 = 0.0;
        // Entry point on the client screen (used to arm the return check)
        let mut entry_x: f64 = 0.0;
        let mut entry_y: f64 = 0.0;
        // Return check is disabled until cursor has moved RETURN_ARM_DISTANCE
        // away from the entry edge — prevents instant Enter→Return flip.
        let mut return_armed = false;
        // Watchdog: auto-disable suppression if no events for too long
        let mut last_forward_time = Instant::now();
        // Diagnostic: throttled cursor position log when not forwarding
        let mut last_cursor_log = Instant::now();
        log::info!(
            "Edge detection config: edge={:?}, server_screen={}x{}",
            config.edge, config.server_screen.width, config.server_screen.height
        );

        loop {
            // Watchdog: release suppression if stuck
            if forwarding && last_forward_time.elapsed() > SUPPRESS_TIMEOUT {
                log::warn!("Suppression watchdog triggered, releasing mouse");
                forwarding = false;
                suppress.store(false, Ordering::SeqCst);
                if cursor_hidden {
                    capture::show_local_cursor();
                    cursor_hidden = false;
                }
                let leave_msg = protocol::serialize(&Message::Leave)?;
                let _ = socket.send_to(&leave_msg, client_addr);
            }

            // Send heartbeat every second
            if last_heartbeat.elapsed() > Duration::from_secs(1) {
                let hb = protocol::serialize(&Message::Heartbeat)?;
                let _ = socket.send_to(&hb, client_addr);
                last_heartbeat = Instant::now();
            }

            // Process captured input events
            match receiver.recv_timeout(Duration::from_millis(1)) {
                Ok(CapturedInput::Key(key_event)) => {
                    // Only forward keys while the mouse is on the client.
                    // When the mouse is on the server, the user's keyboard
                    // belongs to local apps — we don't intercept it.
                    if forwarding {
                        last_forward_time = Instant::now();
                        log::info!(
                            "forwarding key: code={} down={}",
                            key_event.keycode, key_event.down
                        );
                        let msg = protocol::serialize(&Message::KeyInput(key_event))?;
                        let _ = socket.send_to(&msg, client_addr);
                    }
                }
                Ok(CapturedInput::Mouse(event)) => {
                    if !forwarding {
                        // Check if cursor hit the edge
                        let (cx, cy) = capture::get_cursor_position().unwrap_or((0.0, 0.0));
                        // Throttled diagnostic: log cursor position once per second
                        if last_cursor_log.elapsed() > Duration::from_millis(1000) {
                            log::info!(
                                "cursor=({:.0},{:.0}) dx={:.1} dy={:.1} at_edge={}",
                                cx, cy, event.dx, event.dy, config.at_edge(cx, cy)
                            );
                            last_cursor_log = Instant::now();
                        }
                        if config.at_edge(cx, cy) {
                            forwarding = true;
                            suppress.store(true, Ordering::SeqCst);
                            if !cursor_hidden {
                                capture::hide_local_cursor();
                                cursor_hidden = true;
                            }
                            let (ex, ey) = config.entry_position(cx, cy);
                            entry_x = ex;
                            entry_y = ey;
                            client_cursor_x = ex;
                            client_cursor_y = ey;
                            return_armed = false;
                            last_forward_time = Instant::now();
                            let enter_msg =
                                protocol::serialize(&Message::Enter { x: ex, y: ey })?;
                            socket.send_to(&enter_msg, client_addr)?;
                            log::info!("Mouse entered client screen at ({:.0}, {:.0})", ex, ey);
                            // Don't process this same event's delta — it was
                            // the edge-hitting event itself. Wait for the next.
                            continue;
                        }
                    }

                    if forwarding {
                        last_forward_time = Instant::now();

                        // Update virtual cursor position on client
                        client_cursor_x += event.dx;
                        client_cursor_y += event.dy;

                        // Check if cursor hit the return edge on client screen
                        let client_screen = config.client_screen.as_ref()
                            .unwrap_or(&config.server_screen);
                        let cw = client_screen.width as f64;
                        let ch = client_screen.height as f64;

                        // Arm return only after cursor has moved far enough
                        // from the entry point in the expected direction.
                        if !return_armed {
                            let moved_inside = match config.edge {
                                Edge::Left => entry_x - client_cursor_x,
                                Edge::Right => client_cursor_x - entry_x,
                                Edge::Top => entry_y - client_cursor_y,
                                Edge::Bottom => client_cursor_y - entry_y,
                            };
                            if moved_inside >= RETURN_ARM_DISTANCE {
                                return_armed = true;
                            }
                        }

                        let should_return = return_armed && match config.edge {
                            Edge::Right => client_cursor_x <= 0.0,
                            Edge::Left => client_cursor_x >= cw - 1.0,
                            Edge::Bottom => client_cursor_y <= 0.0,
                            Edge::Top => client_cursor_y >= ch - 1.0,
                        };

                        if should_return {
                            forwarding = false;
                            suppress.store(false, Ordering::SeqCst);
                            if cursor_hidden {
                                capture::show_local_cursor();
                                cursor_hidden = false;
                            }
                            let leave_msg = protocol::serialize(&Message::Leave)?;
                            socket.send_to(&leave_msg, client_addr)?;
                            log::info!("Mouse returned to server screen");
                            continue;
                        }

                        // Clamp cursor within client bounds
                        client_cursor_x = client_cursor_x.clamp(0.0, cw - 1.0);
                        client_cursor_y = client_cursor_y.clamp(0.0, ch - 1.0);

                        let msg = protocol::serialize(&Message::Input(event))?;
                        let _ = socket.send_to(&msg, client_addr);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    log::error!("Capture channel disconnected");
                    suppress.store(false, Ordering::SeqCst);
                    if cursor_hidden {
                        capture::show_local_cursor();
                    }
                    break;
                }
            }

            // Check for incoming client messages (non-blocking)
            let mut buf = [0u8; 4096];
            match socket.recv_from(&mut buf) {
                Ok((len, addr)) => {
                    match protocol::deserialize(&buf[..len]) {
                        Ok(Message::Heartbeat) => {}
                        Ok(Message::Hello(_screen)) => {
                            // Client reconnected
                            log::info!("Client reconnected from {}", addr);
                            let ack = protocol::serialize(&Message::HelloAck(
                                get_screen_info().unwrap_or(ScreenInfo { width: 1920, height: 1080 }),
                            ))?;
                            let _ = socket.send_to(&ack, addr);
                        }
                        _ => {}
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
        }
        Ok(())
    }
}
