use std::{
    cmp::min,
    net::IpAddr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use hickory_resolver::Resolver;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::config::{AddressFamily, validate_target_address};

pub const MAX_CACHE_AGE: Duration = Duration::from_hours(1);
const MIN_REFRESH: Duration = Duration::from_mins(1);
const MAX_REFRESH: Duration = Duration::from_mins(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsAnswer {
    pub addresses: Vec<IpAddr>,
    pub ttl: Duration,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DnsStatus {
    pub host: String,
    pub active_address: Option<IpAddr>,
    pub last_success_unix: Option<u64>,
    pub next_refresh_unix: Option<u64>,
    pub cache_written_unix: Option<u64>,
    pub status: Health,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Healthy,
    Degraded,
    Failed,
}

impl DnsStatus {
    #[must_use]
    pub fn new(host: String) -> Self {
        Self {
            host,
            active_address: None,
            last_success_unix: None,
            next_refresh_unix: None,
            cache_written_unix: None,
            status: Health::Failed,
            failure_reason: None,
        }
    }

    #[must_use]
    pub fn cache_age_seconds(&self, now: u64) -> Option<u64> {
        self.cache_written_unix
            .map(|saved| now.saturating_sub(saved))
    }

    #[must_use]
    pub fn can_restore_cache(&self, now: u64) -> bool {
        self.active_address.is_some()
            && self
                .cache_age_seconds(now)
                .is_some_and(|age| age <= MAX_CACHE_AGE.as_secs())
    }

    /// Applies a successful lookup. The current address wins whenever it is
    /// still present, otherwise the lexicographically smallest valid address
    /// wins, making address changes deterministic.
    ///
    /// # Errors
    ///
    /// Returns an error when `answer` contains no permitted address for
    /// `family`.
    pub fn accept_answer(
        &mut self,
        family: AddressFamily,
        answer: DnsAnswer,
        now: u64,
        jitter_seconds: u64,
    ) -> Result<bool> {
        let candidates = valid_candidates(family, answer.addresses, &self.host)?;
        let previous = self.active_address;
        let selected = previous
            .filter(|address| candidates.contains(address))
            .unwrap_or(candidates[0]);
        self.active_address = Some(selected);
        self.last_success_unix = Some(now);
        self.cache_written_unix = Some(now);
        self.next_refresh_unix = Some(now + refresh_delay(answer.ttl, jitter_seconds).as_secs());
        self.status = Health::Healthy;
        self.failure_reason = None;
        Ok(previous != Some(selected))
    }

    pub fn record_failure(&mut self, reason: impl Into<String>, now: u64) {
        self.status = if self.active_address.is_some() {
            Health::Degraded
        } else {
            Health::Failed
        };
        self.failure_reason = Some(reason.into());
        self.next_refresh_unix = Some(now + MIN_REFRESH.as_secs());
    }
}

/// Resolves host names into addresses and supplies their cache lifetime.
pub trait NameResolver: Send + Sync {
    /// Looks up `host`.
    ///
    /// # Errors
    ///
    /// Returns an implementation-defined error when the lookup cannot produce
    /// a DNS answer.
    fn lookup(&self, host: &str) -> Result<DnsAnswer>;
}

#[derive(Debug, Default)]
pub struct SystemResolver;

impl NameResolver for SystemResolver {
    fn lookup(&self, host: &str) -> Result<DnsAnswer> {
        let resolver =
            Resolver::from_system_conf().context("cannot load system resolver configuration")?;
        let answer = resolver
            .lookup_ip(host)
            .with_context(|| format!("DNS lookup failed for {host}"))?;
        // Hickory exposes its cache deadline as a monotonic `Instant`, not a
        // wall-clock `SystemTime`. A saturated difference is also correct for
        // an answer that expires between the lookup and this calculation.
        let ttl = answer
            .valid_until()
            .saturating_duration_since(Instant::now());
        Ok(DnsAnswer {
            addresses: answer.iter().collect(),
            ttl,
        })
    }
}

/// Resolves a target's initial DNS state, optionally restoring fresh cache.
///
/// # Errors
///
/// Returns an error when a literal target is invalid, the resolver fails with
/// no fresh cached address, or an answer contains no permitted address for the
/// requested family.
pub fn resolve_initial(
    host: &str,
    family: AddressFamily,
    cached: Option<&DnsStatus>,
    resolver: &dyn NameResolver,
    now: u64,
) -> Result<DnsStatus> {
    if let Ok(address) = host.parse::<IpAddr>() {
        validate_target_address(family, address, host).map_err(|error| anyhow::anyhow!(error))?;
        let mut status = DnsStatus::new(host.to_owned());
        status.accept_answer(
            family,
            DnsAnswer {
                addresses: vec![address],
                ttl: MAX_REFRESH,
            },
            now,
            0,
        )?;
        return Ok(status);
    }
    match resolver.lookup(host) {
        Ok(answer) => {
            let mut status = cached
                .filter(|state| state.host == host)
                .cloned()
                .unwrap_or_else(|| DnsStatus::new(host.to_owned()));
            status.accept_answer(family, answer, now, random_jitter_seconds())?;
            Ok(status)
        }
        Err(error) => {
            let mut status = cached
                .filter(|state| state.host == host && state.can_restore_cache(now))
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "initial DNS lookup for {host} failed and no fresh cache exists: {error:#}"
                    )
                })?;
            status.status = Health::Degraded;
            status.failure_reason = Some(format!("DNS lookup failed; restored cache: {error:#}"));
            status.next_refresh_unix = Some(now + MIN_REFRESH.as_secs());
            Ok(status)
        }
    }
}

#[must_use]
pub fn refresh_delay(ttl: Duration, jitter_seconds: u64) -> Duration {
    let half = Duration::from_secs(ttl.as_secs() / 2);
    let clamped = half.clamp(MIN_REFRESH, MAX_REFRESH);
    min(
        MAX_REFRESH,
        clamped.saturating_add(Duration::from_secs(jitter_seconds.min(15))),
    )
}

#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn valid_candidates(
    family: AddressFamily,
    addresses: Vec<IpAddr>,
    host: &str,
) -> Result<Vec<IpAddr>> {
    let mut candidates = addresses
        .into_iter()
        .filter(|address| validate_target_address(family, *address, host).is_ok())
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(IpAddr::to_string);
    candidates.dedup();
    if candidates.is_empty() {
        bail!(
            "DNS answer for {host} has no allowed {} address",
            match family {
                AddressFamily::Ipv4 => "IPv4",
                AddressFamily::Ipv6 => "IPv6",
            }
        );
    }
    Ok(candidates)
}

fn random_jitter_seconds() -> u64 {
    rand::thread_rng().gen_range(0..=15)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    #[derive(Default)]
    struct FakeResolver {
        answers: std::sync::Mutex<VecDeque<Result<DnsAnswer>>>,
    }

    impl FakeResolver {
        fn with(answers: Vec<Result<DnsAnswer>>) -> Self {
            Self {
                answers: std::sync::Mutex::new(answers.into()),
            }
        }
    }

    impl NameResolver for FakeResolver {
        fn lookup(&self, _: &str) -> Result<DnsAnswer> {
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("fake resolver exhausted")))
        }
    }

    fn answer(values: &[&str], ttl: u64) -> DnsAnswer {
        DnsAnswer {
            addresses: values.iter().map(|value| value.parse().unwrap()).collect(),
            ttl: Duration::from_secs(ttl),
        }
    }

    #[test]
    fn active_dns_address_is_stable_then_sorted_on_removal() {
        let resolver = FakeResolver::with(vec![Ok(answer(&["198.51.100.9", "198.51.100.2"], 120))]);
        let mut state =
            resolve_initial("example.test", AddressFamily::Ipv4, None, &resolver, 100).unwrap();
        assert_eq!(state.active_address.unwrap().to_string(), "198.51.100.2");
        let changed = state
            .accept_answer(
                AddressFamily::Ipv4,
                answer(&["198.51.100.9", "198.51.100.2"], 120),
                200,
                0,
            )
            .unwrap();
        assert!(!changed);
        let changed = state
            .accept_answer(AddressFamily::Ipv4, answer(&["198.51.100.9"], 120), 300, 0)
            .unwrap();
        assert!(changed);
        assert_eq!(state.active_address.unwrap().to_string(), "198.51.100.9");
    }

    #[test]
    fn failure_is_degraded_with_active_address_and_cached_start_has_one_hour_limit() {
        let resolver = FakeResolver::with(vec![Err(anyhow::anyhow!("offline"))]);
        let cached = DnsStatus {
            host: "example.test".into(),
            active_address: Some("198.51.100.3".parse().unwrap()),
            last_success_unix: Some(1),
            next_refresh_unix: Some(2),
            cache_written_unix: Some(100),
            status: Health::Healthy,
            failure_reason: None,
        };
        let restored = resolve_initial(
            "example.test",
            AddressFamily::Ipv4,
            Some(&cached),
            &resolver,
            3_700,
        )
        .unwrap();
        assert_eq!(restored.status, Health::Degraded);
        assert!(
            resolve_initial(
                "example.test",
                AddressFamily::Ipv4,
                Some(&cached),
                &resolver,
                3_701
            )
            .is_err()
        );
        let mut running = restored;
        running.record_failure("still offline", 3_800);
        assert_eq!(running.status, Health::Degraded);
    }

    #[test]
    fn refresh_is_half_ttl_and_clamped() {
        assert_eq!(refresh_delay(Duration::from_secs(10), 0), MIN_REFRESH);
        assert_eq!(
            refresh_delay(Duration::from_mins(150), 0),
            Duration::from_mins(15)
        );
        assert_eq!(
            refresh_delay(Duration::from_mins(2), 15),
            Duration::from_secs(75)
        );
    }

    #[test]
    fn dns_filters_wrong_family_and_disallowed_addresses() {
        let mut state = DnsStatus::new("example.test".into());
        assert!(
            state
                .accept_answer(AddressFamily::Ipv4, answer(&["::1", "127.0.0.1"], 60), 0, 0)
                .is_err()
        );
    }
}
