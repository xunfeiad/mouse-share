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
        // Enlarge the kernel receive buffer. With the default macOS UDP
        // buffer (~40 KiB) a burst from a 1000 Hz gaming mouse can briefly
        // overflow, causing events to be dropped and the cursor to jitter.
        // 1 MiB is trivial memory and eliminates the drops.
        let _ = socket2::SockRef::from(&socket).set_recv_buffer_size(1 << 20);
        let _ = socket2::SockRef::from(&socket).set_send_buffer_size(1 << 20);
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

            // Accumulate Move deltas across every packet we can drain in
            // this iteration, then emit exactly one simulator.move_relative
            // at the end. Non-Move events (button/scroll/key) are flushed
            // inline in order so click/drag timing is preserved.
            //
            // Why: each simulator.move_relative on macOS costs a
            // CGWarpMouseCursorPosition + CGEvent::post (~hundreds of µs).
            // A 1000 Hz gaming mouse can enqueue dozens of Move packets
            // during one 50 ms recv timeout — processing them one-by-one
            // lets the backlog grow faster than we drain it, which is the
            // visible "client is laggy" symptom. Summing the deltas and
            // issuing one move per drain cycle bounds per-frame work.
            let mut pending_dx: f64 = 0.0;
            let mut pending_dy: f64 = 0.0;
            let mut have_move = false;
            let mut first_pass = true;

            loop {
                // First pass uses the blocking read with the 50 ms timeout
                // so we don't spin the CPU when idle. Subsequent passes
                // flip the socket non-blocking to drain whatever is
                // already queued, then restore blocking mode. SO_RCVTIMEO
                // is preserved across the toggle on macOS/Linux so we
                // don't need to reinstall the read timeout.
                if !first_pass {
                    let _ = socket.set_nonblocking(true);
                }
                let recv_result = socket.recv_from(&mut buf);
                if !first_pass {
                    let _ = socket.set_nonblocking(false);
                }
                first_pass = false;

                match recv_result {
                    Ok((len, _)) => {
                        let msg = match protocol::deserialize(&buf[..len]) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };

                        match msg {
                            Message::Enter { x, y } => {
                                // Flush any accumulated moves before a
                                // teleport — otherwise the relative deltas
                                // would be applied after the absolute jump.
                                if have_move {
                                    let _ = simulator.move_relative(pending_dx, pending_dy);
                                    pending_dx = 0.0;
                                    pending_dy = 0.0;
                                    have_move = false;
                                }
                                active = true;
                                state.mouse_on_peer.store(true, Ordering::SeqCst);
                                sim_x = x;
                                sim_y = y;
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
                                // Flush pending moves so the final cursor
                                // position is correct before we hide it.
                                if have_move {
                                    let _ = simulator.move_relative(pending_dx, pending_dy);
                                    pending_dx = 0.0;
                                    pending_dy = 0.0;
                                    have_move = false;
                                }
                                active = false;
                                state.mouse_on_peer.store(false, Ordering::SeqCst);
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
                                if let Err(e) =
                                    simulator.key_event(key.keycode, key.down, key.flags)
                                {
                                    log::error!("Key simulation error: {}", e);
                                }
                                state.events_total.fetch_add(1, Ordering::Relaxed);
                                state.last_event_ms.store(now_ms(), Ordering::SeqCst);
                            }
                            Message::Input(event) if active => {
                                state.events_total.fetch_add(1, Ordering::Relaxed);
                                state.last_event_ms.store(now_ms(), Ordering::SeqCst);
                                match &event.event_type {
                                    MouseEventType::Move => {
                                        sim_x += event.dx;
                                        sim_y += event.dy;
                                        pending_dx += event.dx;
                                        pending_dy += event.dy;
                                        have_move = true;
                                    }
                                    // Clicks/scrolls must stay ordered
                                    // with respect to moves — flush any
                                    // accumulated delta before applying.
                                    MouseEventType::ButtonDown(btn) => {
                                        if have_move {
                                            let _ = simulator
                                                .move_relative(pending_dx, pending_dy);
                                            pending_dx = 0.0;
                                            pending_dy = 0.0;
                                            have_move = false;
                                        }
                                        if let Err(e) = simulator.button_down(*btn) {
                                            log::error!("Simulation error: {}", e);
                                        }
                                    }
                                    MouseEventType::ButtonUp(btn) => {
                                        if have_move {
                                            let _ = simulator
                                                .move_relative(pending_dx, pending_dy);
                                            pending_dx = 0.0;
                                            pending_dy = 0.0;
                                            have_move = false;
                                        }
                                        if let Err(e) = simulator.button_up(*btn) {
                                            log::error!("Simulation error: {}", e);
                                        }
                                    }
                                    MouseEventType::Scroll { dx, dy } => {
                                        // Scroll applies at the current
                                        // cursor position — flush pending
                                        // moves first so the scroll lands
                                        // on the right target.
                                        if have_move {
                                            let _ = simulator
                                                .move_relative(pending_dx, pending_dy);
                                            pending_dx = 0.0;
                                            pending_dy = 0.0;
                                            have_move = false;
                                        }
                                        if let Err(e) = simulator.scroll(*dx, *dy) {
                                            log::error!("Simulation error: {}", e);
                                        }
                                    }
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
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        // Nothing more queued — stop draining.
                        break;
                    }
                    Err(e) => {
                        log::error!("Network error: {}", e);
                        state.set_error(format!("{}", e));
                        break;
                    }
                }
            }

            // Flush the accumulated move as a single simulator call.
            if have_move {
                if last_move_log.elapsed() > Duration::from_millis(1000) {
                    log::info!(
                        "sim cursor=({:.0},{:.0}) flush dx={:.1} dy={:.1}",
                        sim_x, sim_y, pending_dx, pending_dy
                    );
                    last_move_log = Instant::now();
                }
                if let Err(e) = simulator.move_relative(pending_dx, pending_dy) {
                    log::error!("Simulation error: {}", e);
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
