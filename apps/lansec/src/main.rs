use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "lansec", about = "LAN remote desktop: Capture → Zero-copy → HW Encode → BUD → Frame Timing → HW Decode")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print hardware encode/decode caps, including 4:4:4
    Probe,
    /// Host a desktop session
    Host {
        #[arg(long, default_value = "0.0.0.0:44700")]
        bind: SocketAddr,
        #[arg(long, default_value = "0000")]
        pin: String,
    },
    /// Connect to a host
    Client {
        #[arg(long)]
        connect: SocketAddr,
        #[arg(long, default_value = "0000")]
        pin: String,
    },
    /// Local capture+encode smoke test (no network)
    Loopback,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("lansec=info".parse()?))
        .init();
    match Cli::parse().cmd {
        Cmd::Probe => lansec_session::print_probe(),
        Cmd::Host { bind, pin } => lansec_session::run_host(bind, pin)?,
        Cmd::Client { connect, pin } => lansec_session::run_client(connect, pin)?,
        Cmd::Loopback => lansec_session::run_loopback()?,
    }
    Ok(())
}
