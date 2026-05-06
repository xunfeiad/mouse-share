use crate::config::{Edge, ScreenConfig};
use crate::input::capture::{self, CapturedInput};
use crate::net::state::{now_ms, SharedState};
use crate::protocol::{self, Message, MouseEvent, ScreenInfo};
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

    pub fn run(&self, state: Arc<SharedState>) -> Result<()> {
        state.started_ms.store(now_ms(), Ordering::SeqCst);
        state.clear_error();

        let socket = match UdpSocket::bind(format!("0.0.0.0:{}", self.port)) {
            Ok(s) => s,
            Err(e) => {
                state.set_error(format!("Failed to bind port {}: {}", self.port, e));
                return Err(e.into());
            }
        };
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        // Enlarge kernel send buffer. Mouse event bursts at 500–1000 Hz
        // can briefly exceed the default buffer on macOS/Windows, causing
        // send_to() to drop or block. 1 MiB is plenty for small UDP packets
        // and still trivial compared to process memory.
        let _ = socket2::SockRef::from(&socket).set_send_buffer_size(1 << 20);
        let _ = socket2::SockRef::from(&socket).set_recv_buffer_size(1 << 20);
        log::info!("Server listening on 0.0.0.0:{}", self.port);

        // Start clipboard TCP server on port+1 in background
        let clipboard_port = self.port + 1;
        let clip_shutdown = Arc::new(AtomicBool::new(false));
        let clip_shutdown_for_thread = clip_shutdown.clone();
        let clipboard_thread = std::thread::Builder::new()
            .name("clipboard-server".into())
            .spawn(move || {
                crate::clipboard::run_server(clipboard_port, clip_shutdown_for_thread);
            })?;

        // Wait for client Hello (interruptible)
        let (client_addr, client_screen) = match self.wait_for_client(&socket, &state) {
            Ok(v) => v,
            Err(e) => {
                clip_shutdown.store(true, Ordering::SeqCst);
                let _ = clipboard_thread.join();
                return Err(e);
            }
        };
        if state.shutdown.load(Ordering::SeqCst) {
            clip_shutdown.store(true, Ordering::SeqCst);
            let _ = clipboard_thread.join();
            return Ok(());
        }
        log::info!("Client connected: {} ({:?})", client_addr, client_screen);
        state.connected.store(true, Ordering::SeqCst);
        state.set_peer(client_addr.to_string());
        state.last_heartbeat_ms.store(now_ms(), Ordering::SeqCst);

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

        let capture_shutdown = Arc::new(AtomicBool::new(false));
        let capture_shutdown_for_thread = capture_shutdown.clone();
        let capture_thread = std::thread::Builder::new()
            .name("input-capture".into())
            .spawn(move || {
                if let Err(e) = capturer.run(sender, capture_shutdown_for_thread) {
                    log::error!("Capture error: {}", e);
                }
            })?;

        // Switch to non-blocking for event loop
        socket.set_nonblocking(true)?;

        // Main event loop
        let loop_result = self.event_loop(
            socket,
            client_addr,
            receiver,
            suppress,
            &screen_config,
            &state,
        );

        // Graceful teardown: tell capture + clipboard threads to exit, then
        // join. The event_loop always releases cursor suppression on exit.
        capture_shutdown.store(true, Ordering::SeqCst);
        clip_shutdown.store(true, Ordering::SeqCst);
        state.connected.store(false, Ordering::SeqCst);
        state.mouse_on_peer.store(false, Ordering::SeqCst);
        let _ = capture_thread.join();
        let _ = clipboard_thread.join();

        loop_result
    }

    fn wait_for_client(
        &self,
        socket: &UdpSocket,
        state: &Arc<SharedState>,
    ) -> Result<(SocketAddr, ScreenInfo)> {
        log::info!("Waiting for client to connect...");
        let mut buf = [0u8; 4096];
        loop {
            if state.shutdown.load(Ordering::SeqCst) {
                // Return a sentinel error the caller converts into a clean
                // shutdown path — but the caller also checks `shutdown`
                // right after this, so any placeholder value will do.
                return Ok((
                    "0.0.0.0:0".parse().unwrap(),
                    ScreenInfo { width: 0, height: 0 },
                ));
            }
            match socket.recv_from(&mut buf) {
                Ok((len, addr)) => {
                    if let Ok(Message::Hello(screen)) = protocol::deserialize(&buf[..len]) {
                        return Ok((addr, screen));
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    // The `flush_pending_move!` macro expands its resets in tail position at
    // the end of each drain cycle, where the compiler correctly sees them
    // as dead stores. They're load-bearing for the mid-cycle expansions
    // (when a Button/Scroll/Return flushes accumulated Moves). Allow at
    // the function level to silence the tail-position warnings.
    #[allow(unused_assignments)]
    fn event_loop(
        &self,
        socket: UdpSocket,
        client_addr: SocketAddr,
        receiver: Receiver<CapturedInput>,
        suppress: Arc<AtomicBool>,
        config: &ScreenConfig,
        state: &Arc<SharedState>,
    ) -> Result<()> {
        let mut forwarding = false;
        // Explicit tracking of local cursor visibility. Guarantees hide/show
        // stay paired even across weird state transitions (watchdog, disconnect).
        let mut cursor_hidden = false;
        let mut last_heartbeat = Instant::now();
        // Reusable send buffer for the hot path. Without this, every forwarded
        // mouse event allocates a fresh `Vec<u8>` from `protocol::serialize`.
        // At 500–1000 Hz that adds up — serialize_into reuses capacity.
        let mut send_buf: Vec<u8> = Vec::with_capacity(128);
        // Reusable batch buffer for draining the capture channel. We drain
        // everything currently queued each iteration so consecutive Moves
        // can be coalesced into a single UDP packet. 128 is a generous
        // upper bound on how many events can queue up between iterations
        // even with a 1 kHz gaming mouse.
        let mut event_batch: Vec<CapturedInput> = Vec::with_capacity(128);
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
            // Graceful shutdown request from UI
            if state.shutdown.load(Ordering::SeqCst) {
                log::info!("Server event loop: shutdown requested");
                suppress.store(false, Ordering::SeqCst);
                if cursor_hidden {
                    capture::show_local_cursor();
                }
                let _ = socket.send_to(&protocol::serialize(&Message::Leave)?, client_addr);
                break;
            }

            // Watchdog: release suppression if stuck
            if forwarding && last_forward_time.elapsed() > SUPPRESS_TIMEOUT {
                log::warn!("Suppression watchdog triggered, releasing mouse");
                forwarding = false;
                suppress.store(false, Ordering::SeqCst);
                state.mouse_on_peer.store(false, Ordering::SeqCst);
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

            // Drain the capture channel: one blocking recv (1 ms timeout)
            // plus any additional events already queued. This lets us
            // coalesce consecutive Move events during forwarding into a
            // single UDP packet, cutting the server→client packet rate
            // from ~1 kHz down to the outer-loop iteration rate and giving
            // the client far more headroom per event.
            event_batch.clear();
            match receiver.recv_timeout(Duration::from_millis(1)) {
                Ok(first) => {
                    event_batch.push(first);
                    while event_batch.len() < event_batch.capacity() {
                        match receiver.try_recv() {
                            Ok(more) => event_batch.push(more),
                            Err(_) => break,
                        }
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

            // Accumulator for coalesced Move deltas during forwarding.
            // Flushed before any non-Move event, mode transition, or at
            // the end of the drain cycle.
            let mut pending_dx: f64 = 0.0;
            let mut pending_dy: f64 = 0.0;
            let mut have_pending_move = false;

            // Helper macro to flush the accumulated Move into a single
            // Input packet. Declared as a local closure would borrow
            // send_buf / socket / state, so a macro keeps it ergonomic.
            macro_rules! flush_pending_move {
                () => {
                    if have_pending_move {
                        let ev = MouseEvent::Move {
                            dx: pending_dx,
                            dy: pending_dy,
                        };
                        protocol::serialize_into(&mut send_buf, &Message::Input(ev))?;
                        let _ = socket.send_to(&send_buf, client_addr);
                        state.last_event_ms.store(now_ms(), Ordering::SeqCst);
                        pending_dx = 0.0;
                        pending_dy = 0.0;
                        have_pending_move = false;
                    }
                };
            }

            for captured in event_batch.drain(..) {
                match captured {
                    CapturedInput::Key(key_event) => {
                        // Only forward keys while the mouse is on the
                        // client. When the mouse is on the server, the
                        // user's keyboard belongs to local apps.
                        if forwarding {
                            // Flush pending Moves first so key timing is
                            // preserved relative to cursor position.
                            flush_pending_move!();
                            last_forward_time = Instant::now();
                            log::info!(
                                "forwarding key: code={} down={}",
                                key_event.keycode, key_event.down
                            );
                            protocol::serialize_into(
                                &mut send_buf,
                                &Message::KeyInput(key_event),
                            )?;
                            let _ = socket.send_to(&send_buf, client_addr);
                            state.last_event_ms.store(now_ms(), Ordering::SeqCst);
                        }
                    }
                    CapturedInput::Mouse { event, abs_x, abs_y } => {
                        if !forwarding {
                            // Edge detection path. The capture layer already
                            // read the absolute cursor position from the HID
                            // event, so we don't need a separate
                            // `get_cursor_position()` syscall per event —
                            // that used to be 1 kHz of wasted IPC to the
                            // window server while the mouse was on the
                            // server side.
                            let (cx, cy) = (abs_x, abs_y);
                            if last_cursor_log.elapsed() > Duration::from_millis(1000) {
                                let (edx, edy) = match &event {
                                    MouseEvent::Move { dx, dy } => (*dx, *dy),
                                    _ => (0.0, 0.0),
                                };
                                log::info!(
                                    "cursor=({:.0},{:.0}) dx={:.1} dy={:.1} at_edge={}",
                                    cx, cy, edx, edy, config.at_edge(cx, cy)
                                );
                                last_cursor_log = Instant::now();
                            }
                            if config.at_edge(cx, cy) {
                                forwarding = true;
                                suppress.store(true, Ordering::SeqCst);
                                state.mouse_on_peer.store(true, Ordering::SeqCst);
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
                                // Nothing to flush — we weren't forwarding.
                                let enter_msg = protocol::serialize(&Message::Enter {
                                    x: ex,
                                    y: ey,
                                })?;
                                socket.send_to(&enter_msg, client_addr)?;
                                log::info!(
                                    "Mouse entered client: cursor=({:.0},{:.0}) → entry=({:.0},{:.0}) \
                                     server={}x{} client={} edge={:?}",
                                    cx, cy, ex, ey,
                                    config.server_screen.width, config.server_screen.height,
                                    config.client_screen.as_ref()
                                        .map(|s| format!("{}x{}", s.width, s.height))
                                        .unwrap_or_else(|| "none".into()),
                                    config.edge
                                );
                                // The edge-hitting event itself is not
                                // replayed as a delta — wait for the next.
                                continue;
                            }
                        }

                        if forwarding {
                            last_forward_time = Instant::now();

                            // Cursor tracking: only Move events carry
                            // positional deltas. Button/Scroll events don't
                            // shift the cursor.
                            if let MouseEvent::Move { dx, dy } = &event {
                                client_cursor_x += dx;
                                client_cursor_y += dy;
                            }

                            let client_screen = config
                                .client_screen
                                .as_ref()
                                .unwrap_or(&config.server_screen);
                            let cw = client_screen.width as f64;
                            let ch = client_screen.height as f64;

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

                            let should_return = return_armed
                                && match config.edge {
                                    Edge::Right => client_cursor_x <= 0.0,
                                    Edge::Left => client_cursor_x >= cw - 1.0,
                                    Edge::Bottom => client_cursor_y <= 0.0,
                                    Edge::Top => client_cursor_y >= ch - 1.0,
                                };

                            if should_return {
                                // Flush any accumulated delta up to the
                                // return point before we tell the client
                                // to leave.
                                flush_pending_move!();
                                forwarding = false;
                                suppress.store(false, Ordering::SeqCst);
                                state.mouse_on_peer.store(false, Ordering::SeqCst);
                                if cursor_hidden {
                                    capture::show_local_cursor();
                                    cursor_hidden = false;
                                }
                                let leave_msg = protocol::serialize(&Message::Leave)?;
                                socket.send_to(&leave_msg, client_addr)?;
                                log::info!("Mouse returned to server screen");
                                continue;
                            }

                            client_cursor_x = client_cursor_x.clamp(0.0, cw - 1.0);
                            client_cursor_y = client_cursor_y.clamp(0.0, ch - 1.0);

                            match &event {
                                MouseEvent::Move { dx, dy } => {
                                    // Accumulate — flushed later.
                                    pending_dx += dx;
                                    pending_dy += dy;
                                    have_pending_move = true;
                                }
                                _ => {
                                    // Button / Scroll — preserve ordering
                                    // relative to moves.
                                    flush_pending_move!();
                                    protocol::serialize_into(
                                        &mut send_buf,
                                        &Message::Input(event),
                                    )?;
                                    let _ = socket.send_to(&send_buf, client_addr);
                                    state
                                        .last_event_ms
                                        .store(now_ms(), Ordering::SeqCst);
                                }
                            }
                        }
                    }
                }
            }

            // End of drain cycle: emit a single coalesced Move packet for
            // whatever accumulated this iteration.
            flush_pending_move!();

            // Check for incoming client messages (non-blocking)
            let mut buf = [0u8; 4096];
            match socket.recv_from(&mut buf) {
                Ok((len, addr)) => {
                    match protocol::deserialize(&buf[..len]) {
                        Ok(Message::Heartbeat) => {
                            state.last_heartbeat_ms.store(now_ms(), Ordering::SeqCst);
                        }
                        Ok(Message::Hello(_screen)) => {
                            // Client reconnected
                            log::info!("Client reconnected from {}", addr);
                            state.set_peer(addr.to_string());
                            state.last_heartbeat_ms.store(now_ms(), Ordering::SeqCst);
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
