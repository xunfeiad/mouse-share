mod config;
mod input;
mod net;
mod protocol;
mod screen;

use clap::{Parser, Subcommand};

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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Server { port, edge } => {
            let edge: config::Edge = edge
                .parse()
                .map_err(|e: String| anyhow::anyhow!(e))?;
            log::info!("Starting server on port {}, client edge: {:?}", port, edge);
            let server = net::server::Server::new(port, edge);
            server.run()
        }
        Commands::Client { server } => {
            log::info!("Starting client, connecting to {}", server);
            let client = net::client::Client::new(server);
            client.run()
        }
    }
}
