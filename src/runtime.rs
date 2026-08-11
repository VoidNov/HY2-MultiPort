use std::{
    collections::{BTreeMap, VecDeque},
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use tokio::net::UnixStream;

use crate::{
    config::{Config, Target, ValidatedProfile},
    control::{self, LogEvent, Request, Response},
    dns::{DnsStatus, Health, NameResolver, SystemResolver, resolve_initial, unix_now},
    nft::{NetworkInspector, NftCommand, ResolvedProfile, RouteLocalnetAdjustment, generate_batch},
    state::{self, ProfileStatus, RuntimeState},
};

#[derive(Clone, Debug)]
pub struct DaemonPaths {
    pub config: PathBuf,
    pub socket: PathBuf,
    pub state: PathBuf,
}

impl DaemonPaths {
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            config: PathBuf::from(crate::DEFAULT_CONFIG_PATH),
            socket: PathBuf::from(crate::DEFAULT_SOCKET_PATH),
            state: PathBuf::from(crate::DEFAULT_STATE_PATH),
        }
    }
}

#[derive(Clone, Debug)]
struct ActiveProfile {
    resolved: ResolvedProfile,
    dns: Option<DnsStatus>,
}

#[derive(Debug)]
struct DaemonData {
    state: RuntimeState,
    active: Vec<ActiveProfile>,
    logs: VecDeque<LogEvent>,
}

pub struct Daemon {
    paths: DaemonPaths,
    resolver: Box<dyn NameResolver>,
    nft: NftCommand,
    network: NetworkInspector,
    data: Arc<Mutex<DaemonData>>,
}

impl Daemon {
    /// Creates a daemon with the persisted state cache when it is usable.
    ///
    /// # Errors
    ///
    /// This constructor currently does not return an error: an unreadable or
    /// invalid state cache is logged and replaced with an empty state.
    pub fn new(paths: DaemonPaths) -> Result<Self> {
        let now = unix_now();
        let state = match state::load_optional(&paths.state, now) {
            Ok(state) => state,
            Err(error) => {
                eprintln!("port-forwardd: state cache ignored: {error:#}");
                RuntimeState::empty(now)
            }
        };
        Ok(Self {
            paths,
            resolver: Box::<SystemResolver>::default(),
            nft: NftCommand::default(),
            network: NetworkInspector::default(),
            data: Arc::new(Mutex::new(DaemonData {
                state,
                active: Vec::new(),
                logs: VecDeque::new(),
            })),
        })
    }

    #[must_use]
    pub fn with_commands(mut self, nft_executable: PathBuf, ip_executable: PathBuf) -> Self {
        self.nft = NftCommand::with_executable(nft_executable);
        self.network = NetworkInspector::with_ip_executable(ip_executable);
        self
    }

    /// Starts the daemon event loop until it receives a shutdown signal.
    ///
    /// # Errors
    ///
    /// Returns an error when startup reload, control socket setup or I/O,
    /// signal handling, or control socket cleanup fails.
    pub async fn run(self) -> Result<()> {
        self.log("info", "daemon started; performing startup full reload");
        self.reload("startup")?;
        let listener = control::bind_socket(&self.paths.socket)?;
        self.log("info", "control socket listening");
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tokio::select! {
                accept = listener.accept() => {
                    let (stream, _) = accept?;
                    self.handle_stream(stream).await;
                }
                _ = interval.tick() => {
                    if let Err(error) = self.refresh_due_dns() {
                        self.log("error", format!("DNS refresh cycle failed: {error:#}"));
                    }
                }
                signal = tokio::signal::ctrl_c() => {
                    signal.context("cannot wait for shutdown signal")?;
                    self.log("info", "daemon stopping; committed nftables tables intentionally remain");
                    break;
                }
            }
        }
        if self.paths.socket.exists() {
            std::fs::remove_file(&self.paths.socket)
                .with_context(|| format!("cannot remove socket {}", self.paths.socket.display()))?;
        }
        Ok(())
    }

    async fn handle_stream(&self, mut stream: UnixStream) {
        #[cfg(target_os = "linux")]
        if let Ok(credentials) = stream.peer_cred()
            && credentials.uid() != 0
        {
            let _ = control::send_response(
                &mut stream,
                &Response::failure("control socket accepts root peers only"),
            )
            .await;
            self.log("warn", "rejected non-root Unix socket peer");
            return;
        }
        let response = match control::receive_request(&mut stream).await {
            Ok(Request::Apply) => match self.reload("CLI apply") {
                Ok(()) => Response::success("full reload applied"),
                Err(error) => Response::failure(format!(
                    "full reload rejected; existing rules retained: {error:#}"
                )),
            },
            Ok(Request::Status) => {
                let state = self
                    .data
                    .lock()
                    .expect("daemon state mutex poisoned")
                    .state
                    .clone();
                Response {
                    ok: true,
                    message: "ok".to_owned(),
                    state: Some(state),
                    logs: None,
                }
            }
            Ok(Request::Logs) => Response {
                ok: true,
                message: "in-memory daemon event stream; durable logs are journald/syslog"
                    .to_owned(),
                state: None,
                logs: Some(
                    self.data
                        .lock()
                        .expect("daemon state mutex poisoned")
                        .logs
                        .iter()
                        .cloned()
                        .collect(),
                ),
            },
            Err(error) => Response::failure(error.to_string()),
        };
        if let Err(error) = control::send_response(&mut stream, &response).await {
            self.log(
                "warn",
                format!("failed to send control response: {error:#}"),
            );
        }
    }

    /// Implements the all-or-nothing reload order. Nothing mutates `nftables`,
    /// `route_localnet`, in-memory state, or `state.json` until configuration,
    /// DNS, and local checks and `nft -c` all succeed.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration loading or validation, DNS
    /// resolution, `nftables` preflight or application, network preflight, or
    /// state persistence fails.
    ///
    /// # Panics
    ///
    /// Panics if the daemon state mutex has been poisoned.
    pub fn reload(&self, reason: &str) -> Result<()> {
        let config = Config::from_path(&self.paths.config)?;
        let validated = config.validate().map_err(anyhow::Error::from)?;
        let prior = self
            .data
            .lock()
            .expect("daemon state mutex poisoned")
            .state
            .clone();
        let candidates =
            build_active_profiles(validated, &prior, self.resolver.as_ref(), unix_now())?;
        self.commit_candidates(candidates, &prior, reason)
    }

    fn commit_candidates(
        &self,
        candidates: Vec<ActiveProfile>,
        prior: &RuntimeState,
        reason: &str,
    ) -> Result<()> {
        let resolved = candidates
            .iter()
            .map(|profile| profile.resolved.clone())
            .collect::<Vec<_>>();
        let batch = generate_batch(&resolved);
        self.nft
            .reject_external_hook_conflicts()
            .context("nft hook conflict preflight")?;
        let adjustments = self
            .network
            .preflight(&resolved)
            .context("network preflight")?;
        self.nft.check(&batch)?;
        let adjusted = merge_adjustments(adjustments, &prior.route_localnet_original);
        let fresh = adjusted
            .iter()
            .filter(|adjustment| {
                !prior
                    .route_localnet_original
                    .contains_key(&adjustment.interface)
            })
            .cloned()
            .collect::<Vec<_>>();
        self.network.enable_route_localnet(&adjusted)?;
        if let Err(error) = self.nft.apply(&batch) {
            // Existing loopback profiles remain enabled; only settings newly
            // introduced for this failed reload are rolled back.
            let _ = self.network.restore_route_localnet(&fresh);
            return Err(error);
        }
        let old_to_restore = prior
            .route_localnet_original
            .iter()
            .filter(|(interface, _)| {
                !adjusted
                    .iter()
                    .any(|item| item.interface == interface.as_str())
            })
            .map(|(interface, original)| RouteLocalnetAdjustment {
                interface: interface.clone(),
                original: *original,
            })
            .collect::<Vec<_>>();
        if let Err(error) = self.network.restore_route_localnet(&old_to_restore) {
            self.log(
                "error",
                format!(
                    "nft commit succeeded but stale route_localnet restoration failed: {error:#}"
                ),
            );
        }
        let now = unix_now();
        let mut next = RuntimeState {
            schema_version: 1,
            config_version: prior.config_version.saturating_add(1),
            nft_rules_version: prior.nft_rules_version.saturating_add(1),
            daemon_started_unix: prior.daemon_started_unix,
            updated_unix: now,
            profiles: candidates.iter().map(profile_status).collect(),
            route_localnet_original: adjusted
                .iter()
                .map(|adjustment| (adjustment.interface.clone(), adjustment.original))
                .collect(),
        };
        if next.daemon_started_unix == 0 {
            next.daemon_started_unix = now;
        }
        let save_result = state::save(&self.paths.state, &next);
        {
            let mut data = self.data.lock().expect("daemon state mutex poisoned");
            data.state = next;
            data.active = candidates;
        }
        save_result?;
        self.log("info", format!("configuration reload committed ({reason})"));
        Ok(())
    }

    fn refresh_due_dns(&self) -> Result<()> {
        let now = unix_now();
        let (mut active, prior) = {
            let data = self.data.lock().expect("daemon state mutex poisoned");
            (data.active.clone(), data.state.clone())
        };
        if active.is_empty() {
            return Ok(());
        }
        let before = active.clone();
        let mut changed_destination = false;
        let mut state_changed = false;
        for profile in &mut active {
            let Some(dns) = &mut profile.dns else {
                continue;
            };
            if dns.next_refresh_unix.is_some_and(|due| due > now) {
                continue;
            }
            match self.resolver.lookup(&dns.host) {
                Ok(answer) => match dns.accept_answer(
                    profile.resolved.validated.profile.family,
                    answer,
                    now,
                    0,
                ) {
                    Ok(changed) => {
                        profile.resolved.destination = dns.active_address;
                        changed_destination |= changed;
                        state_changed = true;
                        if changed && let Some(address) = dns.active_address {
                            self.log(
                                "info",
                                format!(
                                    "DNS address switched for profile {} to {}",
                                    profile.resolved.validated.profile.name, address
                                ),
                            );
                        }
                    }
                    Err(error) => {
                        dns.record_failure(error.to_string(), now);
                        state_changed = true;
                        self.log(
                            "warn",
                            format!(
                                "DNS answer rejected for profile {}: {error:#}",
                                profile.resolved.validated.profile.name
                            ),
                        );
                    }
                },
                Err(error) => {
                    dns.record_failure(format!("DNS refresh failed: {error:#}"), now);
                    state_changed = true;
                    self.log(
                        "warn",
                        format!(
                            "DNS refresh failed for profile {}: {error:#}",
                            profile.resolved.validated.profile.name
                        ),
                    );
                }
            }
        }
        if changed_destination {
            // A target switch is another full atomic transaction. On failure,
            // retain the old target/rules and report the affected profiles as
            // degraded rather than claiming a switch that never committed.
            if let Err(error) = self.commit_candidates(active.clone(), &prior, "DNS target switch")
            {
                let mut degraded = before;
                for (current, old) in active.iter().zip(&mut degraded) {
                    if current.resolved.destination != old.resolved.destination
                        && let Some(dns) = &mut old.dns
                    {
                        dns.record_failure(format!("nft target switch failed: {error:#}"), now);
                    }
                }
                self.save_refresh_only(degraded, &prior)?;
                return Err(error);
            }
        } else if state_changed {
            self.save_refresh_only(active, &prior)?;
        }
        Ok(())
    }

    fn save_refresh_only(&self, active: Vec<ActiveProfile>, prior: &RuntimeState) -> Result<()> {
        let mut next = prior.clone();
        next.updated_unix = unix_now();
        next.profiles = active.iter().map(profile_status).collect();
        state::save(&self.paths.state, &next)?;
        let mut data = self.data.lock().expect("daemon state mutex poisoned");
        data.active = active;
        data.state = next;
        Ok(())
    }

    fn log(&self, level: &str, message: impl Into<String>) {
        let event = LogEvent {
            timestamp_unix: unix_now(),
            level: level.to_owned(),
            message: message.into(),
        };
        eprintln!("port-forwardd {} {}", event.level, event.message);
        let mut data = self.data.lock().expect("daemon state mutex poisoned");
        if data.logs.len() == 256 {
            data.logs.pop_front();
        }
        data.logs.push_back(event);
    }
}

fn build_active_profiles(
    validated: Vec<ValidatedProfile>,
    prior: &RuntimeState,
    resolver: &dyn NameResolver,
    now: u64,
) -> Result<Vec<ActiveProfile>> {
    validated
        .into_iter()
        .map(|validated| {
            let cached_dns = prior
                .profiles
                .iter()
                .find(|profile| profile.name == validated.profile.name)
                .and_then(|profile| profile.dns.as_ref());
            let (destination, dns) = match &validated.profile.target {
                Target::Remote { host, .. } => {
                    if let Ok(address) = host.parse::<IpAddr>() {
                        (Some(address), None)
                    } else {
                        let dns = resolve_initial(
                            host,
                            validated.profile.family,
                            cached_dns,
                            resolver,
                            now,
                        )
                        .with_context(|| format!("profile {:?}", validated.profile.name))?;
                        (dns.active_address, Some(dns))
                    }
                }
                Target::Redirect { .. } | Target::LoopbackDnat { .. } => (None, None),
            };
            Ok(ActiveProfile {
                resolved: ResolvedProfile {
                    validated,
                    destination,
                },
                dns,
            })
        })
        .collect()
}

fn merge_adjustments(
    adjustments: Vec<RouteLocalnetAdjustment>,
    prior: &BTreeMap<String, bool>,
) -> Vec<RouteLocalnetAdjustment> {
    adjustments
        .into_iter()
        .map(|mut adjustment| {
            if let Some(original) = prior.get(&adjustment.interface) {
                adjustment.original = *original;
            }
            adjustment
        })
        .collect()
}

fn profile_status(active: &ActiveProfile) -> ProfileStatus {
    let profile = &active.resolved.validated.profile;
    let (target_kind, target_port, target_host, source_mode) = match &profile.target {
        Target::Remote {
            host,
            port,
            source_mode,
        } => ("remote", *port, Some(host.clone()), *source_mode),
        Target::Redirect { port } => ("redirect", *port, None, None),
        Target::LoopbackDnat { port } => {
            ("loopback-dnat", *port, Some("127.0.0.1".to_owned()), None)
        }
    };
    let health = active
        .dns
        .as_ref()
        .map_or(Health::Healthy, |dns| dns.status.clone());
    let failure_reason = active
        .dns
        .as_ref()
        .and_then(|dns| dns.failure_reason.clone());
    ProfileStatus {
        name: profile.name.clone(),
        family: profile.family,
        protocols: profile.protocols.clone(),
        listen_address: profile.listen_address.clone(),
        listen_ports: active.resolved.validated.listen_ports.clone(),
        publicly_exposed: active.resolved.validated.source_cidrs.len() == 1
            && active.resolved.validated.source_cidrs[0] == profile.family.any_source(),
        source_cidrs: active.resolved.validated.source_cidrs.clone(),
        target_kind: target_kind.to_owned(),
        target_port,
        target_host,
        target_address: active
            .resolved
            .target_address()
            .map(|address| address.to_string()),
        source_mode,
        dns: active.dns.clone(),
        health,
        failure_reason,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, time::Duration};

    use crate::{config::AddressFamily, dns::DnsAnswer};

    use super::*;

    struct Resolver(std::sync::Mutex<VecDeque<anyhow::Result<DnsAnswer>>>);

    impl NameResolver for Resolver {
        fn lookup(&self, _: &str) -> Result<DnsAnswer> {
            self.0.lock().unwrap().pop_front().unwrap()
        }
    }

    #[test]
    fn profile_status_marks_default_source_as_public() {
        let validated = Config::from_toml(
            r#"
schema_version = 1
[[profiles]]
name = "dns"
family = "ipv4"
listen_address = "192.0.2.10"
protocols = ["udp"]
[profiles.listen_ports]
ports = [53]
[profiles.target]
kind = "redirect"
port = 5353
"#,
        )
        .unwrap()
        .validate()
        .unwrap()
        .remove(0);
        let status = profile_status(&ActiveProfile {
            resolved: ResolvedProfile {
                validated,
                destination: None,
            },
            dns: None,
        });
        assert!(status.publicly_exposed);
        assert_eq!(status.health, Health::Healthy);
    }

    #[test]
    fn dns_initial_load_uses_cached_profile_by_name() {
        let valid = Config::from_toml(
            r#"
schema_version = 1
[[profiles]]
name = "remote"
family = "ipv4"
listen_address = "192.0.2.10"
protocols = ["tcp"]
[profiles.listen_ports]
ports = [443]
[profiles.target]
kind = "remote"
host = "example.test"
port = 443
source_mode = "preserve"
"#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let resolver = Resolver(std::sync::Mutex::new(VecDeque::from([Ok(DnsAnswer {
            addresses: vec!["198.51.100.2".parse().unwrap()],
            ttl: Duration::from_mins(1),
        })])));
        let profiles =
            build_active_profiles(valid, &RuntimeState::empty(0), &resolver, 10).unwrap();
        assert_eq!(
            profiles[0].resolved.destination.unwrap().to_string(),
            "198.51.100.2"
        );
        assert_eq!(
            profiles[0].resolved.validated.profile.family,
            AddressFamily::Ipv4
        );
    }

    #[test]
    fn merges_original_route_localnet_value_across_reload() {
        let original = BTreeMap::from([("eth0".to_owned(), false)]);
        let merged = merge_adjustments(
            vec![RouteLocalnetAdjustment {
                interface: "eth0".to_owned(),
                original: true,
            }],
            &original,
        );
        assert!(!merged[0].original);
    }
}
