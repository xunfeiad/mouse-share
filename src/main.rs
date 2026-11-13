use clap::{Parser, Subcommand};
use mouse_share::{
    config, input, log_buffer,
    net::{
        session::{Role, SessionCommand, SessionConfig, SessionHandle},
        state::Phase,
    },
    settings,
};
#[derive(Parser)]
#[command(name = "mouse-share", about = "Share mouse across WiFi network")]
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
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    Server {
        #[arg(short, long, default_value_t = 4242)]
        port: u16,
        #[arg(short, long, default_value = "right")]
        edge: String,
    },
    Client {
        #[arg(short, long)]
        server: String,
    },
}
fn main() -> anyhow::Result<()> {
    log_buffer::install();
    let cli = Cli::parse();
    let path = settings::directory()?.join("config.json");
    let (preferences, warning) = settings::load(&path)?;
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    if !path.exists() {
        settings::save(&path, &preferences)?;
    }
    log::set_max_level(if cli.debug || preferences.debug_logging {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    });
    let (role, port, server, edge) = match cli.command {
        Commands::Server { port, edge } => (
            Role::Server,
            port,
            String::new(),
            edge.parse::<config::Edge>().map_err(anyhow::Error::msg)?,
        ),
        Commands::Client { server } => (Role::Client, 4242, server, config::Edge::Right),
    };
    let mut features = preferences.features;
    features.keyboard &= !cli.no_keyboard;
    features.clipboard &= !cli.no_clipboard;
    let config = SessionConfig {
        role,
        port,
        server,
        edge,
        device: preferences.device,
        features,
        auto_reconnect: preferences.auto_reconnect && !cli.no_reconnect,
    };
    config.validate()?;
    input::capture::promote_to_foreground_app();
    input::capture::restore_cursor_state_on_startup();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let requested = stop.clone();
    ctrlc::set_handler(move || requested.store(true, std::sync::atomic::Ordering::SeqCst))?;
    let mut session = SessionHandle::start(config)?;
    let mut phase = None;
    let mut error = None;
    while !session.is_finished() {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            session.command(SessionCommand::Stop);
        }
        let snapshot = session.snapshot();
        if phase != Some(snapshot.phase) {
            eprintln!("{:?}", snapshot.phase);
            phase = Some(snapshot.phase);
        }
        if error != snapshot.error {
            if let Some(e) = &snapshot.error {
                eprintln!("{e}");
            }
            error = snapshot.error;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    session.reap();
    let snapshot = session.snapshot();
    if snapshot.phase == Phase::Failed {
        anyhow::bail!(
            "{}",
            snapshot.error.unwrap_or_else(|| "Session failed".into())
        );
    }
    Ok(())
}
