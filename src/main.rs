use clap::{Parser, Subcommand};
use mouse_share::{config, input, log_buffer, net};

#[derive(Parser)]
#[command(name = "mouse-share", about = "Share mouse across WiFi network")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as server (controller side)
    Server {
        /// Port to listen on
        #[arg(short, long, default_value_t = 4242)]
        port: u16,

        /// Edge where client screen is located (left/right/top/bottom)
        #[arg(short, long, default_value = "right")]
        edge: String,
    },
    /// Run as client (controlled side)
    Client {
        /// Server address (e.g., 192.168.1.100:4242)
        #[arg(short, long)]
        server: String,
    },
}

fn main() -> anyhow::Result<()> {
    // Install the tee logger so both stderr (env_logger) and the in-memory
    // ring buffer (for the GUI Log tab) see every record.
    let _ = log_buffer::install();

    let cli = Cli::parse();

    // Promote to foreground app so CGDisplayHideCursor actually takes
    // effect. Without this, hiding the cursor from a plain CLI binary
    // silently no-ops because macOS treats it as a background process.
    // Both server and client need this (client also hides its cursor
    // when mouse is on server).
    input::capture::promote_to_foreground_app();

    let state = std::sync::Arc::new(net::SharedState::new());

    match cli.command {
        Commands::Server { port, edge } => {
            let edge: config::Edge = edge
                .parse()
                .map_err(|e: String| anyhow::anyhow!(e))?;
            log::info!("Starting server on port {}, client edge: {:?}", port, edge);
            let server = net::server::Server::new(port, edge);
            server.run(state)
        }
        Commands::Client { server } => {
            log::info!("Starting client, connecting to {}", server);
            let client = net::client::Client::new(server);
            client.run(state)
        }
    }
}
