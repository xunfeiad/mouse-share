# mouse-share

[中文版](README.zh-CN.md)

Share mouse control between multiple computers over a WiFi LAN. Cross-platform support for macOS and Windows.

## How it works

```text
Server (controller) ── UDP P ────────▶ Client (target): absolute cursor
                    ◀─ TCP P ───────▶ handshake / heartbeat / reliable input
                    ◀─ TCP P+1 ─────▶ session-bound clipboard
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

## Settings and desktop lifecycle

Versioned `config.json` is stored in `~/Library/Application Support/com.xunfei.mouse-share/` on macOS and `%APPDATA%\MouseShare\` on Windows. Writes are atomic; failures roll back visible settings. Corrupt configurations are preserved as `config.corrupt-*.json`. Tests can use `MOUSE_SHARE_CONFIG_DIR`.

Keyboard, clipboard, reconnect and tray default on; debug logging and login startup default off. Keyboard/clipboard controls on the server’s connected page apply during the session. Disabling keyboard releases remote keys first; disabling clipboard cancels both directions. Port/edge changes apply on the next start; the server decides the edge. Language and logging level apply immediately.

With the tray enabled, closing the window hides it and sharing continues. The tray provides Open, Start, Stop and Quit. Disabling the tray first restores the window; closing then exits. Login startup is opt-in, uses the current user’s LaunchAgent/HKCU Run entry, and passes `--autostart` to restore the saved successful role/configuration. Ordinary launch shows the window. Duplicate GUI launches wake the existing instance.

CLI options: `--no-keyboard`, `--no-clipboard`, `--no-reconnect`, `--debug`. See the [implementation and validation record](doc/16-session-reliability.md) for details and remaining hardware checks.

## Platform permissions

### macOS

Accessibility permission is required:

**System Settings → Privacy & Security → Accessibility** → add the terminal app you run `mouse-share` from (Terminal / iTerm2 / etc.).

### Windows

Input injection is limited by the target application’s integrity level. Failures are reported; see [Microsoft SendInput documentation](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput).

## Architecture

### Project layout

```
src/
├── main.rs                 # CLI entry point, server/client dispatch
├── protocol.rs             # Wire protocol (message types, serialization)
├── config.rs               # Screen config, edge detection
├── screen.rs               # Cross-platform screen info
├── net/
│   ├── session.rs          # Shared GUI/CLI configuration, commands, handle
│   ├── runtime.rs          # Session ownership, forwarding, reconnect, cleanup
│   ├── transport.rs        # Cancellable framed TCP
│   └── state.rs            # Shared snapshots
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

Protocol v2 uses `serde` / `bincode`. TCP frames have a 4-byte big-endian length prefix. UDP carries absolute positions with session, control-generation, movement and control sequence numbers. Upgrade both endpoints together; legacy detection only reports the upgrade requirement.

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

- Base port `P`: `1…65534`, default `4242`. Allow **UDP P, TCP P and TCP P+1** on the server.
- TCP controls use `TCP_NODELAY`. Clicks, wheel, keys and settings are ordered; clicks first apply their associated position and obsolete movement is ignored.
- Heartbeats run every second. A monotonic 5-second peer timeout and immediate TCP-close detection restore input. Idle time does not disconnect; UDP traffic cannot mask a dead control connection.
- Reconnect uses 1, 2, 4, 8-second capped backoff plus jitter. Every connection replaces the full session, including the UDP destination. Explicit stop/cancel/quit or disabling reconnect cancels retries.
- Leaving, disconnecting, stopping and replacing a session release its injected buttons/keys/modifiers and restore local input/cursor visibility.

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
mouse-share --debug server
mouse-share --debug client --server 192.168.1.100:4242
```

## Known limitations

- Only one client connection is supported.
- Connections are unencrypted and unauthenticated; session identifiers isolate stale traffic. Use only on a trusted LAN.
- Keyboard forwarding requires matching operating systems.
- Multiple monitors and holding a button across screen boundaries are not supported.
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
