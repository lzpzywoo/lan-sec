use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use lansec_session::{SessionConfig, SessionMode};

#[derive(Parser)]
#[command(
    name = "lansec",
    about = "LAN remote desktop: Capture → Zero-copy → HW Encode → BUD → Frame Timing → HW Decode",
    subcommand_required = false
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print hardware encode/decode caps, including 4:4:4
    Probe,
    /// Host a desktop session (headless; no GUI)
    Host {
        #[arg(long, default_value = "0.0.0.0:44700")]
        bind: std::net::SocketAddr,
        #[arg(long, default_value = "0000")]
        pin: String,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long, default_value_t = 50)]
        target_mbps: u32,
        #[arg(long, default_value_t = 40)]
        min_mbps: u32,
        #[arg(long, default_value_t = 100)]
        max_mbps: u32,
    },
    /// Connect to a host (no GUI)
    Client {
        #[arg(long)]
        connect: std::net::SocketAddr,
        #[arg(long, default_value = "0000")]
        pin: String,
        #[arg(long, default_value_t = 60)]
        fps: u32,
    },
    /// Local capture+encode smoke test (no network)
    Loopback,
    /// Open the settings launcher (same as running with no subcommand)
    Gui,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("lansec=info".parse()?))
        .init();
    match Cli::parse().cmd {
        None | Some(Cmd::Gui) => match lansec_ui::run_launcher()? {
            Some(cfg) => run_session(cfg)?,
            None => tracing::info!("launcher quit"),
        },
        Some(Cmd::Probe) => lansec_session::print_probe(),
        Some(Cmd::Host {
            bind,
            pin,
            fps,
            target_mbps,
            min_mbps,
            max_mbps,
        }) => {
            let mut cfg = SessionConfig::for_host(bind, pin);
            cfg.target_fps = fps;
            cfg.target_mbps = target_mbps;
            cfg.min_mbps = min_mbps;
            cfg.max_mbps = max_mbps;
            cfg.clamp();
            lansec_session::run_host(cfg)?;
        }
        Some(Cmd::Client { connect, pin, fps }) => {
            let mut cfg = SessionConfig::for_client(connect, pin);
            cfg.target_fps = fps;
            cfg.clamp();
            lansec_session::run_client(cfg)?;
        }
        Some(Cmd::Loopback) => lansec_session::run_loopback()?,
    }
    Ok(())
}

fn run_session(cfg: SessionConfig) -> Result<()> {
    match cfg.mode {
        SessionMode::Host => lansec_session::run_host(cfg),
        SessionMode::Client => lansec_session::run_client(cfg),
    }
}
