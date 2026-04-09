use crate::config::{Edge, ScreenConfig};
use crate::input::capture;
use crate::protocol::{self, Message, MouseEvent, ScreenInfo};
use crate::screen::get_screen_info;
use anyhow::Result;
use crossbeam_channel::{bounded, Receiver};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SUPPRESS_TIMEOUT: Duration = Duration::from_secs(5);

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
        let (sender, receiver) = bounded::<MouseEvent>(256);

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
        receiver: Receiver<MouseEvent>,
        suppress: Arc<AtomicBool>,
        config: &ScreenConfig,
    ) -> Result<()> {
        let mut forwarding = false;
        let mut last_heartbeat = Instant::now();
        // Track virtual cursor position on the client screen
        let mut client_cursor_x: f64 = 0.0;
        let mut client_cursor_y: f64 = 0.0;
        // Watchdog: auto-disable suppression if no events for too long
        let mut last_forward_time = Instant::now();

        loop {
            // Watchdog: release suppression if stuck
            if forwarding && last_forward_time.elapsed() > SUPPRESS_TIMEOUT {
                log::warn!("Suppression watchdog triggered, releasing mouse");
                forwarding = false;
                suppress.store(false, Ordering::SeqCst);
                let leave_msg = protocol::serialize(&Message::Leave)?;
                let _ = socket.send_to(&leave_msg, client_addr);
            }

            // Send heartbeat every second
            if last_heartbeat.elapsed() > Duration::from_secs(1) {
                let hb = protocol::serialize(&Message::Heartbeat)?;
                let _ = socket.send_to(&hb, client_addr);
                last_heartbeat = Instant::now();
            }

            // Process captured mouse events
            match receiver.recv_timeout(Duration::from_millis(1)) {
                Ok(event) => {
                    if !forwarding {
                        // Check if cursor hit the edge
                        let (cx, cy) = capture::get_cursor_position().unwrap_or((0.0, 0.0));
                        if config.at_edge(cx, cy) {
                            forwarding = true;
                            suppress.store(true, Ordering::SeqCst);
                            let (entry_x, entry_y) = config.entry_position(cx, cy);
                            client_cursor_x = entry_x;
                            client_cursor_y = entry_y;
                            last_forward_time = Instant::now();
                            let enter_msg =
                                protocol::serialize(&Message::Enter { x: entry_x, y: entry_y })?;
                            socket.send_to(&enter_msg, client_addr)?;
                            log::info!("Mouse entered client screen at ({:.0}, {:.0})", entry_x, entry_y);
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

                        let should_return = match config.edge {
                            Edge::Right => client_cursor_x <= 0.0,
                            Edge::Left => client_cursor_x >= cw - 1.0,
                            Edge::Bottom => client_cursor_y <= 0.0,
                            Edge::Top => client_cursor_y >= ch - 1.0,
                        };

                        if should_return {
                            forwarding = false;
                            suppress.store(false, Ordering::SeqCst);
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
