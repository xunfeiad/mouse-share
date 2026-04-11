# mouse-share

[中文版](README.zh-CN.md)

Share mouse control between multiple computers over a WiFi LAN. Cross-platform support for macOS and Windows.

## How it works

```
┌─────────────────┐              ┌─────────────────┐
│ Server (control)│── UDP ──────▶│ Client (target) │
│ Capture input   │  input evts  │ Simulate input  │
│ Edge detection  │◀── UDP ─────│ Heartbeat       │
└─────────────────┘              └─────────────────┘
```

- **Server**: runs on the controlling machine, captures global mouse events. When the cursor hits the configured screen edge, control switches to the client.
- **Client**: runs on the controlled machine, receives events from the server and simulates them locally.

## Quick start

### Build

```bash
# Requires the Rust toolchain (https://rustup.rs/)
cargo build --release
```

The CLI binary is at `target/release/mouse-share`.

### GUI (macOS)

A small GUI wraps the server/client in a single window. On macOS the
recommended way to run it is as a `.app` bundle — Accessibility permission is
granted to the bundle identifier and survives rebuilds:

```bash
./scripts/build-app.sh
open "dist/mouse share.app"
```

You can also run the GUI directly from the tree without bundling:

```bash
cargo run --release --features ui --bin mouse-share-ui
```

The first launch will prompt for Accessibility. Grant it in **System Settings
→ Privacy & Security → Accessibility**, then relaunch.

### CLI usage

**On the controlling machine (Server):**

```bash
# Default port 4242, client placed on the right
mouse-share server

# Custom port and client edge
mouse-share server --port 5000 --edge left
```

**On the controlled machine (Client):**

```bash
# Connect to the server IP and port
mouse-share client --server 192.168.1.100:4242
```

### Arguments

```
Usage: mouse-share <COMMAND>

Commands:
  server  Run as server (controller side)
  client  Run as client (controlled side)

Server options:
  -p, --port <PORT>  Listening port [default: 4242]
  -e, --edge <EDGE>  Client screen edge: left/right/top/bottom [default: right]

Client options:
  -s, --server <ADDR>  Server address, e.g. 192.168.1.100:4242
```

## Platform permissions

### macOS

Accessibility permission is required:

**System Settings → Privacy & Security → Accessibility** → add the terminal app you run `mouse-share` from (Terminal / iTerm2 / etc.).

### Windows

**Run as Administrator** — low-level mouse hooks require administrator privileges.

## Architecture

### Project layout

```
src/
├── main.rs                 # CLI entry point, server/client dispatch
├── protocol.rs             # Wire protocol (message types, serialization)
├── config.rs               # Screen config, edge detection
├── screen.rs               # Cross-platform screen info
├── net/
│   ├── server.rs           # Server: event forwarding, edge detection
│   └── client.rs           # Client: event receive + simulate
└── input/
    ├── capture.rs           # InputCapture trait
    ├── simulate.rs          # InputSimulator trait
    ├── macos_capture.rs     # macOS: CGEventTap capture
    ├── macos_simulate.rs    # macOS: CGEvent simulation
    ├── win_capture.rs       # Windows: SetWindowsHookEx capture
    └── win_simulate.rs      # Windows: SendInput simulation
```

### Core modules

#### Protocol layer (`protocol.rs`)

`serde` + `bincode` binary serialization. A single mouse event is around 40 bytes on the wire.

```rust
enum Message {
    Hello(ScreenInfo),          // Client → Server: register
    HelloAck(ScreenInfo),       // Server → Client: ack
    Enter { x: f64, y: f64 },  // Cursor entered client screen
    Leave,                      // Cursor left client screen
    Input(MouseEvent),          // Mouse event payload
    Heartbeat,                  // Keepalive
}
```

#### Input capture (`input/capture.rs`)

Cross-platform differences are hidden behind a trait:

| Platform | Implementation | Event suppression |
|----------|----------------|-------------------|
| macOS | `CGEventTap` (HID level) | Return `None` from callback |
| Windows | `SetWindowsHookEx(WH_MOUSE_LL)` | Return `LRESULT(1)` |

#### Input simulation (`input/simulate.rs`)

| Platform | Implementation |
|----------|----------------|
| macOS | `CGEvent::new_mouse_event` + `CGEvent::post` |
| Windows | `SendInput` + `MOUSEINPUT` (normalized absolute coordinates 0–65535) |

### Network design

- **Transport**: UDP — mouse events are high frequency (60–120 Hz), each event is self-contained, and loss is acceptable (the next frame overwrites the state).
- **Connection management**: Hello/HelloAck handshake, the client retries up to 10 times on startup.
- **Keepalive**: bidirectional heartbeat (1 s interval).
- **Safety watchdog**: the server releases input suppression after 5 s of no activity so the mouse can never get permanently stuck.

### Screen switching logic

1. Server continuously tracks the real cursor position.
2. Cursor reaches the configured edge (e.g. right edge) → enable suppression → send `Enter` to the client.
3. Server tracks the virtual cursor position on the client screen.
4. Virtual cursor reaches the return edge on the client → disable suppression → send `Leave`.

## Cross compilation

### Build a Windows binary from macOS

```bash
# Add the Windows target
rustup target add x86_64-pc-windows-msvc
# Use cargo-xwin, or build directly on a Windows machine
```

### Build on Windows

```bash
cargo build --release
```

## Debugging

Enable verbose logs:

```bash
RUST_LOG=debug mouse-share server
RUST_LOG=debug mouse-share client --server 192.168.1.100:4242
```

## Known limitations

- Only one client connection is supported.
- UDP is unencrypted — only use it on a trusted LAN.
- Linux is not supported.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` | CLI argument parsing |
| `serde` + `bincode` | Binary serialization |
| `crossbeam-channel` | Lock-free inter-thread channels |
| `core-graphics` | macOS input capture / simulation |
| `windows` | Windows Win32 API |
| `anyhow` | Error handling |
| `log` + `env_logger` | Logging |

## License

MIT
