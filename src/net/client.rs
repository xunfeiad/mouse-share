use crate::input::{capture, simulate};
use crate::net::state::{now_ms, SharedState};
use crate::protocol::{self, Message, MouseEventType};
use crate::screen::{get_display_refresh_hz, get_screen_info};
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
        // Instant-based rate limiter for cursor warps, sized to the
        // actual display refresh rate instead of a hard-coded 125 Hz.
        //
        // Why tie it to the display rate: `CGWarpMouseCursorPosition` is
        // IPC to the window server, whose cursor pipeline is clocked to
        // the physical display. Warping faster than the display refreshes
        // just backlogs the pipeline — the extra warps are coalesced by
        // the compositor into the same frame and the visible cursor
        // lags behind reality. Warping slower than refresh wastes
        // motion resolution. Matching makes the cursor feel as smooth
        // as the hardware allows: 60 Hz on an office monitor, 120 Hz on
        // ProMotion, 240 Hz on a gaming display — all handled correctly
        // without user-visible tuning knobs.
        //
        // Mouse polling rate is *unrelated*: a 1000 Hz gaming mouse
        // still generates 1000 events/s, all captured by the server and
        // sent to the client. The client coalesces whatever arrived in
        // a drain cycle into one warp, so the per-warp delta scales
        // with mouse rate but the warp *frequency* is bounded by the
        // display. No motion is lost, just batched into bigger hops
        // that land exactly one per display frame.
        let refresh_hz = get_display_refresh_hz();
        let min_warp_interval = Duration::from_secs_f64(1.0 / refresh_hz);
        log::info!(
            "Client warp rate limit: {:.0} Hz ({} ms per flush)",
            refresh_hz,
            min_warp_interval.as_millis()
        );
        let mut last_warp = Instant::now() - min_warp_interval;

        let loop_result: Result<()> = 'evt: loop {
            // Graceful shutdown request from UI
            if state.shutdown.load(Ordering::SeqCst) {
                log::info!("Client event loop: shutdown requested");
                break 'evt Ok(());
            }

            // Send heartbeat + check server liveness (once per second)
            if last_heartbeat.elapsed() > Duration::from_secs(1) {
                let hb = protocol::serialize(&Message::Heartbeat)?;
                let _ = socket.send_to(&hb, &self.server_addr);
                last_heartbeat = Instant::now();

                // If no heartbeat received from server for >5 s, presume
                // it's offline and stop gracefully.
                let last_hb = state.last_heartbeat_ms.load(Ordering::SeqCst);
                if last_hb > 0 && now_ms() - last_hb > 5000 {
                    log::warn!("No heartbeat from server for >5 s, disconnecting");
                    state.set_error("Server offline (heartbeat timeout)".to_string());
                    break 'evt Ok(());
                }
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

            // Drain strategy: one blocking recv with the 50 ms read
            // timeout so we don't spin when idle, then (if we got
            // something) flip to non-blocking ONCE and drain whatever
            // else is already queued, then flip back to blocking.
            //
            // The previous revision toggled `set_nonblocking` on every
            // iteration inside the drain loop, which meant two fcntl
            // syscalls per received packet — ~2000/sec under a 1 kHz
            // gaming mouse, entirely wasted. Toggling once per burst
            // drops that to ~2 × (burst rate) = ~100–200/sec regardless
            // of mouse rate, which is negligible.
            let mut recv_result = socket.recv_from(&mut buf);
            let mut nonblocking_mode = false;

            loop {
                match recv_result {
                    Ok((len, _)) => {
                        // We have at least one packet — if we haven't
                        // already, flip the socket to non-blocking so the
                        // follow-up drain recvs return WouldBlock
                        // immediately instead of waiting 50 ms for packets
                        // that may not come.
                        if !nonblocking_mode {
                            let _ = socket.set_nonblocking(true);
                            nonblocking_mode = true;
                        }

                        let msg = match protocol::deserialize(&buf[..len]) {
                            Ok(m) => m,
                            Err(_) => {
                                recv_result = socket.recv_from(&mut buf);
                                continue;
                            }
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
                                state.last_event_ms.store(now_ms(), Ordering::SeqCst);
                            }
                            Message::Input(event) if active => {
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

                        // Grab the next packet for the drain iteration.
                        recv_result = socket.recv_from(&mut buf);
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

            // Restore blocking mode for the next outer-loop recv. We only
            // flipped it if we actually received at least one packet, so
            // this is a no-op on idle iterations.
            if nonblocking_mode {
                let _ = socket.set_nonblocking(false);
            }

            // Flush the accumulated move as a single simulator call.
            if have_move {
                // Rate-limit cursor warps to ~125 Hz using an Instant
                // budget rather than a fixed post-flush sleep. Why not a
                // fixed sleep: `CGWarpMouseCursorPosition` is IPC to the
                // window server, whose cursor pipeline is clocked to the
                // display (~60–120 Hz). Firing warps faster than that
                // backlogs the pipeline and the visible cursor lags
                // behind — the original "feels like low frame rate" bug.
                // A *fixed* 8 ms sleep fixes that symptom but quantises
                // slow motion into visible 8 ms steps regardless of how
                // long the drain cycle itself took, which the user sees
                // as periodic jitter. Tracking the real time since the
                // last warp and only sleeping the remainder gives smooth
                // 125 Hz output: fast drain cycles still cap at 125 Hz,
                // slow drain cycles pass through with zero added latency.
                let since = last_warp.elapsed();
                if since < min_warp_interval {
                    std::thread::sleep(min_warp_interval - since);
                }

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
                last_warp = Instant::now();
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
