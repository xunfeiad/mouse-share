use crate::input::simulate;
use crate::protocol::{self, Message, MouseEventType};
use crate::screen::get_screen_info;
use anyhow::Result;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

pub struct Client {
    server_addr: String,
}

impl Client {
    pub fn new(server_addr: String) -> Self {
        Self { server_addr }
    }

    pub fn run(&self) -> Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        log::info!("Connecting to server at {}", self.server_addr);

        // Get local screen info
        let screen = get_screen_info()?;
        log::info!("Client screen: {}x{}", screen.width, screen.height);

        // Send Hello with retries
        let hello = protocol::serialize(&Message::Hello(screen.clone()))?;
        let server_screen = self.connect_with_retry(&socket, &hello)?;
        log::info!(
            "Connected to server (screen: {}x{})",
            server_screen.width,
            server_screen.height
        );

        // Create simulator
        let mut simulator = simulate::create_simulator();

        // Event loop
        let mut buf = [0u8; 4096];
        let mut active = false;
        let mut last_heartbeat = Instant::now();

        loop {
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
                            log::info!("Mouse entered at ({:.0}, {:.0})", x, y);
                            if let Err(e) = simulator.move_to(x, y) {
                                log::error!("Failed to move cursor: {}", e);
                            }
                        }
                        Message::Leave => {
                            active = false;
                            log::info!("Mouse left client screen");
                        }
                        Message::Input(event) if active => {
                            let result = match &event.event_type {
                                MouseEventType::Move => {
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
                        Message::Heartbeat => {}
                        _ => {}
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    log::error!("Network error: {}", e);
                }
            }
        }
    }

    fn connect_with_retry(
        &self,
        socket: &UdpSocket,
        hello: &[u8],
    ) -> Result<crate::protocol::ScreenInfo> {
        let mut buf = [0u8; 4096];
        for attempt in 0..10 {
            if attempt > 0 {
                log::info!("Retrying Hello (attempt {}/10)...", attempt + 1);
            }
            let _ = socket.send_to(hello, &self.server_addr);

            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
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
