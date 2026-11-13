use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mouse_share::{
    config::Edge,
    input::capture,
    log_buffer,
    net::{
        self,
        devices::{display_id, DeviceId, Devices, Features, Invite},
        SessionCommand, SessionConfig, SessionRole, SharedState,
    },
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    name = "mouse-share",
    about = "Share mouse and keyboard with a paired LAN device"
)]
struct Cli {
    #[arg(long, global = true)]
    no_keyboard: bool,
    #[arg(long, global = true)]
    no_clipboard: bool,
    #[arg(long, global = true)]
    no_reconnect: bool,
    #[arg(long, global = true)]
    debug: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the controller. Pairing is available while the server runs.
    Server {
        #[arg(short, long, default_value_t = 4242)]
        port: u16,
        #[arg(short, long, default_value = "right")]
        edge: Edge,
        /// Generate an invitation for this address or hostname. Printed once on the terminal.
        #[arg(long)]
        invite_for: Option<String>,
    },
    /// Connect to a saved server by device ID.
    Client {
        #[arg(long, value_parser = parse_id)]
        device: DeviceId,
    },
    /// Read a one-time invitation from the terminal and remember the server.
    Pair,
    /// List saved and authorized devices.
    Devices,
    /// Remove authorization and credentials for a device.
    Revoke {
        #[arg(value_parser = parse_id)]
        device: DeviceId,
    },
    /// Update a saved server's address without changing its certificate.
    Address {
        #[arg(value_parser = parse_id)]
        device: DeviceId,
        address: String,
    },
}

fn parse_id(text: &str) -> Result<DeviceId, String> {
    if text.len() != 32 || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("Expected a 32-character device ID".into());
    }
    let mut id = [0; 16];
    for (byte, hex) in id.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(hex).unwrap(), 16).unwrap();
    }
    Ok(id)
}

fn main() -> Result<()> {
    log_buffer::install();
    let devices = Devices::system()?;
    let cli = Cli::parse();
    let (role, invite) = match cli.command {
        Command::Pair => {
            let code = zeroize::Zeroizing::new(rpassword::prompt_password("Pairing invitation: ")?);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let server = runtime.block_on(net::pair(devices, Invite::decode(&code)?))?;
            println!("Paired with {} ({})", server.name, display_id(server.id));
            return Ok(());
        }
        Command::Devices => {
            for device in devices.list()? {
                println!(
                    "{}  {}  {}",
                    display_id(device.id),
                    device.name,
                    device.address.as_deref().unwrap_or("authorized client")
                );
            }
            return Ok(());
        }
        Command::Revoke { device } => return devices.forget(device),
        Command::Address { device, address } => return devices.set_address(device, address),
        Command::Server {
            port,
            edge,
            invite_for,
        } => (SessionRole::Server { port, edge }, invite_for),
        Command::Client { device } => {
            let server = devices
                .list()?
                .into_iter()
                .find(|d| d.id == device && d.address.is_some())
                .context("Unknown saved server. Use mouse-share devices or pair first.")?;
            (SessionRole::Client { server }, None)
        }
    };

    capture::promote_to_foreground_app();
    capture::restore_cursor_state_on_startup();
    let preferences = mouse_share::settings::directory()
        .ok()
        .and_then(|dir| mouse_share::settings::load(&dir.join("config.json")).ok())
        .map(|(settings, warning)| {
            if let Some(warning) = warning {
                eprintln!("{warning}");
            }
            settings
        })
        .unwrap_or_default();
    log_buffer::set_debug(cli.debug || preferences.debug_logging);
    let state = Arc::new(SharedState::new());
    state.set_auto_reconnect(!cli.no_reconnect && preferences.auto_reconnect);
    let (tx, rx) = mpsc::channel(16);
    let stop = state.clone();
    ctrlc::set_handler(move || stop.request_stop())?;
    if let Some(host) = invite {
        use std::io::IsTerminal;
        anyhow::ensure!(
            std::io::stdout().is_terminal(),
            "Use an interactive terminal to display a pairing invitation"
        );
        tx.try_send(SessionCommand::Invite(host))?;
        let state = state.clone();
        std::thread::spawn(move || {
            for _ in 0..100 {
                if let Some(invite) = state.invitation.lock().unwrap().as_ref() {
                    println!(
                        "Pairing invitation (valid for five minutes):\n{}",
                        invite.as_str()
                    );
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }
    let _keep_open = tx;
    let feature_limits = Features {
        keyboard: !cli.no_keyboard,
        clipboard: !cli.no_clipboard,
    };
    net::run(
        SessionConfig {
            devices,
            role,
            feature_limits,
        },
        state,
        rx,
    )
}
