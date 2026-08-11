use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;
use hy2_multiport::runtime::{Daemon, DaemonPaths};

#[derive(Debug, Parser)]
#[command(name = "port-forwardd", about = "HY2-MultiPort root nftables daemon")]
struct Args {
    #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
    config: PathBuf,
    #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
    socket: PathBuf,
    #[arg(long, default_value = hy2_multiport::DEFAULT_STATE_PATH)]
    state: PathBuf,
    #[arg(long, default_value = "nft")]
    nft: PathBuf,
    #[arg(long, default_value = "ip")]
    ip: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !effective_root()? {
        bail!("port-forwardd must run as root");
    }
    Daemon::new(DaemonPaths {
        config: args.config,
        socket: args.socket,
        state: args.state,
    })?
    .with_commands(args.nft, args.ip)
    .run()
    .await
}

fn effective_root() -> Result<bool> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    Ok(status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .and_then(|line| line.split_whitespace().nth(2))
        == Some("0"))
}
