use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hy2_multiport::{
    config::Config,
    control::{self, Request},
    dns::{Health, unix_now},
    state::{self, RuntimeState},
};

#[derive(Debug, Parser)]
#[command(name = "port-forward", about = "HY2-MultiPort control CLI")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Parse and semantically validate TOML without changing the system.
    Validate {
        #[arg(long, default_value = hy2_multiport::DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Ask the root daemon to run one all-or-nothing full reload.
    Apply {
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Display daemon status; JSON has the stable `RuntimeState` schema.
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long, default_value = hy2_multiport::DEFAULT_STATE_PATH)]
        state: PathBuf,
    },
    /// Retrieve the daemon's in-memory event stream; system logs remain in journald/syslog.
    Logs {
        #[arg(long, default_value = hy2_multiport::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::Validate { config } => {
            let profiles = Config::from_path(&config)?.validate()?;
            println!("valid: schema_version=1, profiles={}", profiles.len());
        }
        Command::Apply { socket } => {
            let response = control::call(socket, &Request::Apply).await?;
            println!("{}", response.message);
        }
        Command::Status {
            json,
            socket,
            state,
        } => {
            let runtime = match control::call(&socket, &Request::Status).await {
                Ok(response) => response.state.context("daemon returned no status")?,
                Err(error) => {
                    let cached = state::load(&state).with_context(|| {
                        format!("daemon unavailable ({error:#}); cached state is also unavailable")
                    })?;
                    mark_refresh_expired(cached, unix_now())
                }
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&runtime)?);
            } else {
                print!("{}", state::render_human(&runtime, unix_now()));
            }
        }
        Command::Logs { socket } => {
            let response = control::call(socket, &Request::Logs).await?;
            for event in response.logs.unwrap_or_default() {
                println!("{} {} {}", event.timestamp_unix, event.level, event.message);
            }
        }
    }
    Ok(())
}

fn mark_refresh_expired(mut state: RuntimeState, now: u64) -> RuntimeState {
    for profile in &mut state.profiles {
        if profile
            .dns
            .as_ref()
            .and_then(|dns| dns.next_refresh_unix)
            .is_some_and(|due| due <= now)
        {
            profile.health = Health::Degraded;
            profile.failure_reason = Some("daemon unavailable; DNS refresh is overdue".to_owned());
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use hy2_multiport::{dns::DnsStatus, state::ProfileStatus};

    use super::*;

    #[test]
    fn cached_status_marks_overdue_dns_when_daemon_is_down() {
        let mut state = RuntimeState::empty(0);
        state.profiles.push(ProfileStatus {
            name: "p".into(),
            family: hy2_multiport::config::AddressFamily::Ipv4,
            protocols: vec![],
            listen_address: "192.0.2.1".into(),
            listen_ports: vec![],
            source_cidrs: vec![],
            publicly_exposed: true,
            target_kind: "remote".into(),
            target_port: 1,
            target_host: Some("x".into()),
            target_address: Some("198.51.100.1".into()),
            source_mode: None,
            dns: Some(DnsStatus {
                host: "x".into(),
                active_address: Some("198.51.100.1".parse().unwrap()),
                last_success_unix: None,
                next_refresh_unix: Some(1),
                cache_written_unix: Some(0),
                status: Health::Healthy,
                failure_reason: None,
            }),
            health: Health::Healthy,
            failure_reason: None,
        });
        assert_eq!(
            mark_refresh_expired(state, 2).profiles[0].health,
            Health::Degraded
        );
    }
}
