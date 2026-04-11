# Performance: server → client lag fix

This doc explains the four hot-path optimizations made on the
`feature/optimze` branch and why each of them matters.

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

## What each fix handles

| Fix                           | Addresses                                 |
|-------------------------------|-------------------------------------------|
| CGEventSource cache (client)  | Per-event CPU cost on client              |
| Associate once                | Per-event syscall / IPC cost on client    |
| Client Move coalescing        | Backlog amplification on the receiver     |
| serialize_into                | Allocator churn on server                 |
| Server Move coalescing        | UDP packet rate fanout from the sender    |
| get_cursor_position cache     | Per-event CPU cost on server              |
| Socket buffers                | Packet drops under burst                  |

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
2. On the client, `RUST_LOG=info` shows flushed move summaries once per
   second:
   ```
   sim cursor=(1234,567) flush dx=12.0 dy=-8.0
   ```
3. Do a fast mouse flick across the client screen. The remote cursor
   should track in real time without visible delay or stutter.
