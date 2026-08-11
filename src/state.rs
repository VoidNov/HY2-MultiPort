use std::{fmt::Write as _, fs, io::Write, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    config::{AddressFamily, Protocol, SourceMode},
    dns::{DnsStatus, Health},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeState {
    pub schema_version: u32,
    pub config_version: u64,
    pub nft_rules_version: u64,
    pub daemon_started_unix: u64,
    pub updated_unix: u64,
    #[serde(default)]
    pub profiles: Vec<ProfileStatus>,
    /// Original `route_localnet` values, retained so explicit cleanup can put
    /// only daemon-modified per-interface settings back.
    #[serde(default)]
    pub route_localnet_original: std::collections::BTreeMap<String, bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileStatus {
    pub name: String,
    pub family: AddressFamily,
    pub protocols: Vec<Protocol>,
    pub listen_address: String,
    pub listen_ports: Vec<u16>,
    pub source_cidrs: Vec<String>,
    pub publicly_exposed: bool,
    pub target_kind: String,
    pub target_port: u16,
    pub target_host: Option<String>,
    pub target_address: Option<String>,
    pub source_mode: Option<SourceMode>,
    pub dns: Option<DnsStatus>,
    pub health: Health,
    pub failure_reason: Option<String>,
}

impl RuntimeState {
    #[must_use]
    pub fn empty(now: u64) -> Self {
        Self {
            schema_version: 1,
            config_version: 0,
            nft_rules_version: 0,
            daemon_started_unix: now,
            updated_unix: now,
            profiles: Vec::new(),
            route_localnet_original: std::collections::BTreeMap::new(),
        }
    }
}

/// Loads the persisted daemon state from a JSON file.
///
/// # Errors
///
/// Returns an error when the file cannot be read or does not contain valid
/// state JSON.
pub fn load(path: impl AsRef<Path>) -> Result<RuntimeState> {
    let path = path.as_ref();
    let data = fs::read(path).with_context(|| format!("cannot read state {}", path.display()))?;
    serde_json::from_slice(&data)
        .with_context(|| format!("invalid state JSON in {}", path.display()))
}

/// Loads persisted state, returning an empty state when no file exists.
///
/// # Errors
///
/// Returns an error when the state file exists but cannot be read or parsed.
pub fn load_optional(path: impl AsRef<Path>, now: u64) -> Result<RuntimeState> {
    let path = path.as_ref();
    match load(path) {
        Ok(state) => Ok(state),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            Ok(RuntimeState::empty(now))
        }
        Err(error) => Err(error),
    }
}

/// Atomically writes persisted daemon state with owner-only permissions.
///
/// # Errors
///
/// Returns an error when the state directory or temporary file cannot be
/// created, state cannot be serialized or synced, permissions cannot be set,
/// or the temporary file cannot replace the destination.
pub fn save(path: impl AsRef<Path>, state: &RuntimeState) -> Result<()> {
    let path = path.as_ref();
    let parent = path
        .parent()
        .context("state path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create state directory {}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    let encoded = serde_json::to_vec_pretty(state)?;
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("cannot create temporary state {}", temporary.display()))?;
        file.write_all(&encoded)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&temporary, path)
        .with_context(|| format!("cannot replace state {}", path.display()))?;
    Ok(())
}

#[must_use]
pub fn render_human(state: &RuntimeState, now: u64) -> String {
    let mut output = format!(
        "config version: {}\nnft rules version: {}\nprofiles: {}\n",
        state.config_version,
        state.nft_rules_version,
        state.profiles.len()
    );
    for profile in &state.profiles {
        let target = profile
            .target_address
            .as_deref()
            .or(profile.target_host.as_deref())
            .unwrap_or("-");
        let cache_age = profile
            .dns
            .as_ref()
            .and_then(|dns| dns.cache_age_seconds(now))
            .map_or_else(|| "-".to_owned(), |age| format!("{age}s"));
        let _ = writeln!(
            output,
            "- {}: {:?} {:?} {}:{:?} -> {}:{} [{}{}; cache {}]",
            profile.name,
            profile.family,
            profile.protocols,
            profile.listen_address,
            profile.listen_ports,
            target,
            profile.target_port,
            match profile.health {
                Health::Healthy => "healthy",
                Health::Degraded => "degraded",
                Health::Failed => "failed",
            },
            if profile.publicly_exposed {
                ", public"
            } else {
                ""
            },
            cache_age
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip_uses_private_mode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let state = RuntimeState::empty(10);
        save(&path, &state).unwrap();
        assert_eq!(load(&path).unwrap(), state);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
