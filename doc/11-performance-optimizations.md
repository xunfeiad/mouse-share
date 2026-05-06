# Performance: server → client lag fix

This doc explains the hot-path optimizations made on the
`feature/optimze` branch and why each of them matters. The fixes
landed in three rounds:

- **Round 1** (fixes 1–4): per-event CPU cost on the client + allocator
  churn + socket-buffer overruns. Made it noticeably better but not
  smooth.
- **Round 2** (fixes 5–6): server-side Move coalescing +
  `get_cursor_position` source cache. Improvements measurable but the
  user reported **the client still felt laggy while CPU was flat**.
- **Round 3** (fixes 7–8, see the bottom): the actual root cause —
  window-server IPC pipeline backlog. This is the one that finally
  made the motion visibly smooth.

If you're only reading one section, read round 3. The earlier rounds
were necessary but not sufficient — they reduced cost, but the final
symptom wasn't about cost, it was about pipeline backpressure between
our process and WindowServer.

## Symptom

When the server controlled the client, the remote cursor felt visibly
laggy and stuttery — especially with a gaming mouse polling at 500–1000
Hz. The server side was fine; the client was falling behind.

## Root causes (all compounding)

### 1. Per-event `CGEventSource` allocation on the client

Every call that simulated a mouse or key event on macOS went through:

```rust
CGEventSource::new(CGEventSourceStateID::HIDSystemState)?
```

`CGEventSourceCreate` is not cheap — each call allocates and initializes
an event source in the window server (measured in the hundreds of
microseconds per call). At 1000 events/s that's a meaningful fraction of
a CPU core spent on nothing but allocator churn, and the cost scales
linearly with the mouse poll rate.

**Fix** — cache a single `CGEventSource` in the `MacOsSimulator` struct
at construction time and clone it per call. `core-graphics`'
`CGEventSource::clone` is a single `CFRetain` (an atomic increment), so
the per-event cost drops to effectively zero.

```rust
pub struct MacOsSimulator {
    // ...
    source: CGEventSource,
}

impl MacOsSimulator {
    pub fn new() -> Self {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .expect("failed to create CGEventSource (HIDSystemState)");
        // ...
    }

    fn post_mouse_event(&self, /* ... */) -> Result<()> {
        let event = CGEvent::new_mouse_event(self.source.clone(), /* ... */)?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}
```

`CGEventSource` wraps a `NonNull` and therefore isn't `Send` by default,
but the simulator is owned by exactly one thread (the client event loop)
and Core Foundation reference counting is thread-safe, so
`unsafe impl Send for MacOsSimulator {}` is sound in this context and is
required by `Box<dyn InputSimulator>`.

### 2. `CGAssociateMouseAndMouseCursorPosition(true)` on every move

The old `warp_cursor` helper called `CGAssociateMouseAndMouseCursorPosition(true)`
once per simulated move. This is a **mode toggle**, not a per-event
operation — it tells the window server "couple the visible cursor to
mouse events" until told otherwise. Calling it per move does an
unnecessary IPC round-trip to the window server and, worse, starves the
incoming event tap under burst load.

**Fix** — call it exactly once in `MacOsSimulator::new()`.

### 3. No Move-event coalescing on the client

The old client loop read one UDP packet per iteration and called
`simulator.move_relative(dx, dy)` directly. When the server fires a
burst of Move packets (a fast mouse flick), the client has to execute
every single one sequentially — each costing one `CGWarpMouseCursorPosition`
+ one `CGEvent::post`. The packet queue grows faster than the simulator
drains it, and the visible cursor ends up lagging behind the actual
mouse movement by tens of milliseconds.

**Fix** — drain the UDP socket on every outer-loop iteration. After the
initial blocking read returns, the socket is flipped into non-blocking
mode and drained in a tight loop. Move deltas are summed into
`pending_dx` / `pending_dy` instead of being simulated one-by-one. A
single `simulator.move_relative(pending_dx, pending_dy)` is issued at
the end of the drain cycle.

Ordering is preserved for non-Move events:

- `ButtonDown` / `ButtonUp` / `Scroll` — flush pending moves first,
  then apply (so clicks land at the right position).
- `Enter` — flush, then `move_to(x, y)` teleport (so the absolute jump
  isn't followed by stale relative deltas).
- `Leave` — flush, then hide local cursor.

This bounds per-outer-iteration work at one `move_relative` call no
matter how many Move packets arrived in that window.

### 4. Fresh `Vec<u8>` allocation per forwarded event on the server

`protocol::serialize(&msg)` returns a new `Vec<u8>` every call. The
server forwards every mouse event as one of these — at 1000 Hz, that's
one allocator call per millisecond just for the wire buffer.

**Fix** — a reusable `send_buf: Vec<u8>` in `event_loop` and a new
`protocol::serialize_into(&mut buf, &msg)` helper that clears the
buffer and writes into its existing capacity. Now the hot path does
zero heap allocations per event.

### 5. One UDP packet per captured event on the server

Client-side Move coalescing helped, but the server was still emitting
one UDP datagram per captured Move event (~1 kHz with a gaming mouse).
Even though each `recv_from` + `deserialize` on the client is cheap in
isolation, at that rate the per-packet kernel crossing and channel
chatter eats real wall-clock time, and `serialize` + `send_to` on the
server side scales the same way. The server was essentially fanning
the raw mouse poll rate straight onto the wire.

**Fix** — drain the capture channel per outer-loop iteration and
coalesce consecutive Move events into a single `MouseEvent` before
serialization. The server now:

1. Blocks for ≤1 ms on `receiver.recv_timeout`.
2. On a wakeup, drains all additional events already queued with
   `receiver.try_recv` into a reusable `Vec<CapturedInput>` (capacity
   128, a safe upper bound for one drain cycle at 1 kHz).
3. Walks the batch, accumulating Move deltas into `pending_dx` /
   `pending_dy`. A local `flush_pending_move!` macro emits the summed
   Move as one packet.
4. Non-Move events (Button, Scroll, Key, Enter, Leave, Return) call
   `flush_pending_move!` *before* sending their own packet so ordering
   is preserved: a click always lands at the right cursor position.

Return-edge detection still runs per-event inside the drain — the
virtual cursor `client_cursor_x/y` is updated on every Move so a
return crossing in the middle of a batch isn't missed.

```rust
// Drain
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
    _ => {}
}

// Coalesce
let mut pending_dx = 0.0;
let mut pending_dy = 0.0;
let mut have_pending_move = false;

macro_rules! flush_pending_move {
    () => {
        if have_pending_move {
            let ev = MouseEvent::now(pending_dx, pending_dy, MouseEventType::Move);
            protocol::serialize_into(&mut send_buf, &Message::Input(ev))?;
            let _ = socket.send_to(&send_buf, client_addr);
            pending_dx = 0.0;
            pending_dy = 0.0;
            have_pending_move = false;
        }
    };
}

for captured in event_batch.drain(..) {
    // ... accumulate Move into pending, flush before Button/Scroll/Key/return
}
flush_pending_move!();  // tail flush
```

The packet rate cap is now set by the outer loop, not by the raw HID
poll rate. With a 1 ms channel timeout the server emits at most
~1000 Move packets/s, but in practice each iteration drains multiple
events so the rate is much lower under fast motion — which is exactly
when a backlog would form.

### 6. Per-call `CGEventSource` in `get_cursor_position`

The server's edge-detection path calls `capture::get_cursor_position()`
on every mouse event while the mouse is on the *server*. The old macOS
implementation created a fresh `CGEventSource` on every call — same
~hundreds-of-microseconds cost as fix #1, just on the server side.

**Fix** — `thread_local!` cache of `RefCell<Option<CGEventSource>>` so
the source is created once per thread (only the event_loop thread
calls this) and cloned cheaply on every call.

```rust
thread_local! {
    static SOURCE: RefCell<Option<CGEventSource>> = const { RefCell::new(None) };
}

SOURCE.with(|cell| {
    let mut slot = cell.borrow_mut();
    if slot.is_none() {
        *slot = Some(CGEventSource::new(CGEventSourceStateID::HIDSystemState)?);
    }
    let source = slot.as_ref().unwrap().clone();
    let event = CGEvent::new(source)?;
    let pos = event.location();
    Ok((pos.x, pos.y))
})
```

## Round 3 — the actual root cause

After rounds 1 and 2 the *cost* per event was very low, but the user
still reported the client feeling laggy — and crucially, CPU was flat
the whole time. "CPU is idle, but it feels like a low frame rate" is
the exact quote. That sentence is the fingerprint of a pipeline
backpressure problem, not a throughput problem.

### 7. Redundant `post_mouse_event` on every move

The old `MacOsSimulator::move_relative` / `move_to` were doing **two**
window-server IPCs per call:

```rust
fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
    self.current_x += dx;
    self.current_y += dy;
    let point = self.current_point();
    warp_cursor(point);                                          // IPC #1
    self.post_mouse_event(CGEventType::MouseMoved, point, ...)   // IPC #2
    Ok(())
}
```

`CGWarpMouseCursorPosition` is async — it drops a cursor-position
update into WindowServer's queue and returns. `CGEvent::post` is the
same pattern — it enqueues an event in WindowServer's HID event
pipeline. Neither one costs us any measurable CPU, because both
functions return almost immediately.

**But WindowServer does not consume those queues at arbitrary rates.**
Its cursor update cadence is clocked to display refresh (~60–120 Hz).
At a 1 kHz input rate the queue grows by ~880 entries per second. The
visible cursor is *behind* the real mouse by however many entries are
queued, which grows linearly with time until the user stops moving.

Why was the `post` there in the first place? To notify apps that tap
`CGEventTap` for `MouseMoved`. That's a tiny audience (input
recorders, some accessibility tools, a few games) and in exchange
every move was costing a second IPC *into the more congested of the
two pipelines*. The HID event tap queue in particular is strictly
ordered — warps can in principle coalesce (a newer warp position
supersedes an older one), but posted events must process in order.

**Fix** — drop `post_mouse_event` from `move_relative` / `move_to`.
Keep only `warp_cursor`. WindowServer's own cursor tracking (menu
hover, `NSTrackingArea`, hit testing, Dock magnification) picks up
cursor position from the warp directly; apps that specifically needed
a synthetic `MouseMoved` tap event stop getting it, which is the
accepted tradeoff. This is the same pattern Synergy / Barrier /
input-leap use.

This roughly halves the per-move IPC load, and more importantly
removes the `post` path entirely — the one that couldn't coalesce.

### 8. Client-side flush rate cap (~125 Hz)

Even with only `warp_cursor` per move, the client was still calling
it at packet arrival rate, which on a LAN with a 1 kHz mouse is…
~1 kHz. That's still too fast for WindowServer to keep up with.
Coalescing in the drain loop would have helped, except that on a LAN
UDP packets arrive one-at-a-time — when the drain loop wakes from
`recv_from`, there's usually exactly one packet queued, no batching
opportunity.

The fix is to **deliberately wait** between flushes, giving the
kernel time to accumulate more packets, then drain and coalesce a
real batch. In `net::client::run`:

```rust
if have_move {
    if let Err(e) = simulator.move_relative(pending_dx, pending_dy) {
        log::error!("Simulation error: {}", e);
    }
    // Rate-limit cursor warps to ~125 Hz (8 ms per flush).
    // CGWarpMouseCursorPosition is a non-blocking IPC to the window
    // server — it returns fast, but the window server processes cursor
    // moves on its own pipeline clocked to the display refresh rate
    // (~60–120 Hz). Sending warps faster than that backlogs the
    // pipeline, and the visible cursor lags the real mouse by
    // however much has accumulated.
    //
    // Sleeping 8 ms after a flush:
    //   1. Caps the per-second warp count at ~125.
    //   2. Lets the kernel UDP buffer accumulate more packets, which
    //      the next drain cycle coalesces into one larger warp. No
    //      motion is dropped, just batched at display rate.
    //
    // Non-move events (click/scroll/key/Enter/Leave) already flush
    // pending moves inline and execute before we reach this sleep,
    // so click latency is unaffected.
    std::thread::sleep(Duration::from_millis(8));
}
```

The key property: **this does not drop events**. Mouse motion that
happens during the 8 ms sleep is preserved in the kernel UDP buffer
(which we enlarged to 1 MiB precisely for this) and gets folded into
the next warp as a summed delta. The cursor ends up in the same place
as if we had issued a warp per packet — it just travels there in one
move instead of 8.

The key question: *does this add 8 ms of click latency?* No. The
drain loop processes non-Move events (button, scroll, key, Enter,
Leave, Return) **inline**, flushing any pending move before executing
them. By the time the loop reaches the `if have_move { ... sleep }`
branch, all the clicks/scrolls in the current batch are already done.
The sleep only delays *the next batch of moves*.

### Why profilers missed this

This is worth remembering: **CPU profilers do not see cross-process
IPC backlog**. `perf` / Instruments / samply all measure CPU time in
our own process. `warp_cursor` returns in microseconds — the wait is
happening in WindowServer, on a different thread in a different
process, driven by the display link. From our process's vantage
point, we spent effectively no time on cursor moves. From the user's
vantage point, the cursor is 300 ms behind the mouse.

The diagnostic signature that pointed at this: **"high visible
latency, flat CPU, and the lag grows with motion speed rather than
with event count"**. If the CPU had been pegged, round 1's cache and
coalescing fixes would have shown up as CPU drops — but CPU was
already flat, so there was nothing to drop. The lag was purely in
WindowServer's pipeline length.

## UDP socket buffers

Both sides also call:

```rust
socket2::SockRef::from(&socket).set_recv_buffer_size(1 << 20);
socket2::SockRef::from(&socket).set_send_buffer_size(1 << 20);
```

The default UDP socket buffer on macOS is small (~40 KiB). A brief
burst from a high-poll-rate mouse could overflow it and get silently
dropped, appearing to the user as a momentary freeze or jump. 1 MiB is
trivial memory and eliminates the drops.

Note: this buffer is **also** load-bearing for the 8 ms flush rate
cap above — during the sleep, the kernel has to hold ~8 packets at
1 kHz without dropping any. 40 KiB would be enough strictly by packet
count, but 1 MiB gives plenty of margin for occasional scheduling
hiccups pushing the sleep past 8 ms.

## What each fix handles

| Round | Fix                                | Addresses                                     |
|-------|------------------------------------|-----------------------------------------------|
| 1     | `CGEventSource` cache (client sim) | Per-event CPU cost on client                  |
| 1     | Associate cursor once              | Per-event syscall/IPC cost on client          |
| 1     | Client Move coalescing             | Backlog amplification on the receiver         |
| 1     | `serialize_into`                   | Allocator churn on server                     |
| 1     | Socket buffer 1 MiB                | Packet drops under burst (+ sleep buffering)  |
| 2     | Server Move coalescing             | UDP packet rate fanout from the sender        |
| 2     | `get_cursor_position` cache        | Per-event CPU cost on server                  |
| **3** | **Drop `post_mouse_event` on move**| **Halves per-move IPCs; cuts the serialized HID-event pipeline path** |
| **3** | **8 ms client flush rate cap**     | **Caps warp rate at ~125 Hz to match WindowServer's cursor pipeline; eliminates backlog** |

Rounds 1 and 2 are *necessary* — without them, round 3's fixes alone
would still leave the client behind because the per-event cost was
too high to hit the flush cadence cleanly. Round 3 is *sufficient* —
it's the fix that actually made the motion smooth end-to-end.

## What was deliberately NOT changed

- **Wire format**. `MouseEvent` is already small (≈40 bytes on the
  wire). Changing it would break protocol compatibility for marginal
  gain.
- **UDP → TCP or QUIC**. UDP is the right transport: mouse events are
  self-contained, high-frequency, and loss-tolerant (the next frame
  overwrites state).
- **`WinSimulator`**. The Windows `SendInput` path does not have the
  event-source allocation issue. The client-side Move coalescing
  benefits Windows too, since it's platform-agnostic code.
- **Server capture thread**. The bounded(256) `crossbeam_channel` and
  `CGEventTap` pipeline were not a bottleneck under measurement.

## How to verify

1. Run the server and client on two machines on the same LAN.
2. Use a high-poll-rate mouse (500–1000 Hz) on the server side. A
   low-poll-rate mouse will not trigger the bug at all — the
   WindowServer pipeline only starts backing up above ~200 Hz, and
   all the issues above were invisible under slow manual testing.
3. On the client, `RUST_LOG=info` shows flushed move summaries once
   per second:
   ```
   sim cursor=(1234,567) flush dx=12.0 dy=-8.0
   ```
4. Do a fast mouse flick across the client screen. The remote cursor
   should track in real time — it arrives at its destination in one
   visible step, not in a trailing cloud of intermediate positions.
5. Watch client CPU: it should stay flat (low single digits). If CPU
   is high, something in rounds 1–2 regressed. If CPU is flat **and
   motion is laggy**, round 3's fixes are not active (check that the
   `sleep(8)` is still in place and that `post_mouse_event` is no
   longer called from `move_relative`).
