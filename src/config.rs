use std::{
    collections::{BTreeSet, HashSet},
    fs,
    net::IpAddr,
    path::Path,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read configuration {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("configuration error: {0}")]
    Semantic(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Config {
    pub schema_version: u32,
    /// Allows explicitly opting into coexistence with external nftables base
    /// chains. Omitted means `false`.
    #[serde(default)]
    pub allow_external_chains: bool,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub family: AddressFamily,
    pub listen_address: String,
    pub protocols: Vec<Protocol>,
    #[serde(default)]
    pub source_cidrs: Vec<String>,
    #[serde(default)]
    pub allow_route_localnet: bool,
    pub listen_ports: ListenPorts,
    pub target: Target,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Ord, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum AddressFamily {
    Ipv4,
    Ipv6,
}

impl AddressFamily {
    #[must_use]
    pub fn matches(self, address: IpAddr) -> bool {
        matches!(
            (self, address),
            (Self::Ipv4, IpAddr::V4(_)) | (Self::Ipv6, IpAddr::V6(_))
        )
    }

    #[must_use]
    pub const fn any_source(self) -> &'static str {
        match self {
            Self::Ipv4 => "0.0.0.0/0",
            Self::Ipv6 => "::/0",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Ord, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    #[must_use]
    pub const fn nft_name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ListenPorts {
    pub range_start: Option<u32>,
    pub range_end: Option<u32>,
    pub suffix: Option<u32>,
    pub ports: Option<Vec<u16>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Target {
    Remote {
        host: String,
        port: u16,
        #[serde(default)]
        source_mode: Option<SourceMode>,
    },
    Redirect {
        port: u16,
    },
    LoopbackDnat {
        port: u16,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceMode {
    Masquerade,
    Preserve,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedProfile {
    pub profile: Profile,
    pub listen_ip: IpAddr,
    pub listen_ports: Vec<u16>,
    pub source_cidrs: Vec<String>,
}

impl Config {
    /// Loads, parses, and validates a configuration file.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Read`] when the file cannot be read,
    /// [`ConfigError::Toml`] when its TOML is invalid, or
    /// [`ConfigError::Semantic`] when it violates the configuration rules.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let content = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml(&content)
    }

    /// Parses and validates TOML configuration content.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Toml`] when `content` is not valid TOML, or
    /// [`ConfigError::Semantic`] when the parsed configuration violates the
    /// configuration rules.
    pub fn from_toml(content: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validates every profile and returns its normalized form.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Semantic`] when the schema version, a profile,
    /// or the collection of listeners violates the configuration rules.
    pub fn validate(&self) -> Result<Vec<ValidatedProfile>, ConfigError> {
        if self.schema_version != 1 {
            return Err(ConfigError::Semantic(format!(
                "schema_version must be 1, got {}",
                self.schema_version
            )));
        }
        let mut names = HashSet::new();
        let mut occupied = HashSet::new();
        self.profiles
            .iter()
            .map(|profile| {
                if profile.name.trim().is_empty() {
                    return Err(ConfigError::Semantic(
                        "profile name may not be empty".to_owned(),
                    ));
                }
                if !names.insert(profile.name.clone()) {
                    return Err(ConfigError::Semantic(format!(
                        "duplicate profile name {:?}",
                        profile.name
                    )));
                }
                let validated = profile.validate()?;
                for protocol in &validated.profile.protocols {
                    for port in &validated.listen_ports {
                        let key = (
                            validated.profile.family,
                            *protocol,
                            validated.listen_ip,
                            *port,
                        );
                        if !occupied.insert(key) {
                            return Err(ConfigError::Semantic(format!(
                                "overlapping listener {} {:?} {}:{}",
                                family_name(validated.profile.family),
                                protocol,
                                validated.listen_ip,
                                port
                            )));
                        }
                    }
                }
                Ok(validated)
            })
            .collect()
    }
    /// Performs the additional checks required before a configuration can be
    /// deployed. Documentation-only addresses are intentionally accepted by
    /// [`Self::validate`] so examples and rule rendering remain testable, but
    /// they must never pass a CLI/daemon startup path.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is incomplete or contains a
    /// documentation-reserved listener, source, or literal remote target.
    pub fn validate_deployable(&self) -> Result<Vec<ValidatedProfile>, ConfigError> {
        let profiles = self.validate()?;
        if profiles.is_empty() {
            return Err(ConfigError::Semantic(
                "profiles must contain at least one complete forwarding profile; run port-forward init"
                    .to_owned(),
            ));
        }
        for validated in &profiles {
            let profile = &validated.profile;
            reject_placeholder(&profile.name, &profile.name, "name")?;
            reject_placeholder(&profile.name, &profile.listen_address, "listen_address")?;
            if is_documentation_address(validated.listen_ip) {
                return Err(ConfigError::Semantic(format!(
                    "profile {:?} listen_address {} is a documentation-reserved address; replace it with an address assigned to this host",
                    profile.name, validated.listen_ip
                )));
            }
            for source in &validated.source_cidrs {
                reject_placeholder(&profile.name, source, "source_cidrs")?;
                let address = source
                    .split('/')
                    .next()
                    .unwrap_or_default()
                    .parse()
                    .map_err(|_| {
                        ConfigError::Semantic(format!(
                            "profile {:?} has invalid source CIDR {source:?}",
                            profile.name
                        ))
                    })?;
                if is_documentation_address(address) {
                    return Err(ConfigError::Semantic(format!(
                        "profile {:?} source CIDR {source:?} uses a documentation-reserved address; replace source_cidrs",
                        profile.name
                    )));
                }
            }
            if let Target::Remote { host, .. } = &profile.target {
                reject_placeholder(&profile.name, host, "target.host")?;
                if let Ok(address) = host.parse::<IpAddr>()
                    && is_documentation_address(address)
                {
                    return Err(ConfigError::Semantic(format!(
                        "profile {:?} remote target {address} is documentation-reserved; replace target.host with a real destination",
                        profile.name
                    )));
                }
            }
        }
        Ok(profiles)
    }
}

impl Profile {
    fn validate(&self) -> Result<ValidatedProfile, ConfigError> {
        let listen_ip: IpAddr = self.listen_address.parse().map_err(|_| {
            ConfigError::Semantic(format!(
                "profile {:?} has invalid listen_address {:?}",
                self.name, self.listen_address
            ))
        })?;
        if !self.family.matches(listen_ip) {
            return Err(ConfigError::Semantic(format!(
                "profile {:?} listen_address does not match family",
                self.name
            )));
        }
        let protocols: BTreeSet<_> = self.protocols.iter().copied().collect();
        if protocols.is_empty() {
            return Err(ConfigError::Semantic(format!(
                "profile {:?} protocols must be a non-empty tcp/udp set",
                self.name
            )));
        }
        if protocols.len() != self.protocols.len() {
            return Err(ConfigError::Semantic(format!(
                "profile {:?} protocols contains duplicates",
                self.name
            )));
        }
        let listen_ports = self.listen_ports.project()?;
        let source_cidrs = validate_source_cidrs(self.family, &self.source_cidrs, &self.name)?;
        match &self.target {
            Target::Remote {
                host, source_mode, ..
            } => {
                if host.trim().is_empty() {
                    return Err(ConfigError::Semantic(format!(
                        "profile {:?} remote host may not be empty",
                        self.name
                    )));
                }
                if self.family == AddressFamily::Ipv4 && source_mode.is_none() {
                    return Err(ConfigError::Semantic(format!(
                        "profile {:?} IPv4 remote target requires source_mode",
                        self.name
                    )));
                }
                if self.family == AddressFamily::Ipv6 && source_mode.is_some() {
                    return Err(ConfigError::Semantic(format!(
                        "profile {:?} IPv6 remote target forbids source_mode/NAT66",
                        self.name
                    )));
                }
                if let Ok(address) = host.parse::<IpAddr>() {
                    validate_target_address(self.family, address, &self.name)?;
                }
            }
            Target::Redirect { .. } => {}
            Target::LoopbackDnat { .. } => {
                if self.family != AddressFamily::Ipv4 {
                    return Err(ConfigError::Semantic(format!(
                        "profile {:?} loopback-dnat is IPv4 only; use redirect for IPv6",
                        self.name
                    )));
                }
                if !self.allow_route_localnet {
                    return Err(ConfigError::Semantic(format!(
                        "profile {:?} loopback-dnat requires allow_route_localnet = true",
                        self.name
                    )));
                }
            }
        }
        Ok(ValidatedProfile {
            profile: self.clone(),
            listen_ip,
            listen_ports,
            source_cidrs,
        })
    }
}

impl ListenPorts {
    /// Projects a numeric suffix across a right-open port range. `suffix = 443`
    /// means decimal ports ending in `443`; its width is the number of decimal
    /// digits (and a zero suffix has width one).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Semantic`] when the supplied range or explicit
    /// port list is incomplete, invalid, duplicated, or produces no ports.
    pub fn project(&self) -> Result<Vec<u16>, ConfigError> {
        let range_values = [self.range_start, self.range_end, self.suffix];
        let has_any_range = range_values.iter().any(Option::is_some);
        match (has_any_range, &self.ports) {
            (true, Some(_)) => Err(ConfigError::Semantic(
                "listen_ports range_start/range_end/suffix and ports are mutually exclusive"
                    .to_owned(),
            )),
            (false, None) => Err(ConfigError::Semantic(
                "listen_ports needs either range_start/range_end/suffix or ports".to_owned(),
            )),
            (true, None) => {
                let (Some(start), Some(end), Some(suffix)) =
                    (self.range_start, self.range_end, self.suffix)
                else {
                    return Err(ConfigError::Semantic(
                        "range port projection requires range_start, range_end, and suffix"
                            .to_owned(),
                    ));
                };
                if !(1..=65_535).contains(&start) || !(2..=65_536).contains(&end) || start >= end {
                    return Err(ConfigError::Semantic(format!(
                        "invalid right-open port range [{start}, {end})"
                    )));
                }
                let width = decimal_width(suffix);
                let modulus = 10_u32.pow(width);
                if suffix >= modulus {
                    return Err(ConfigError::Semantic("invalid port suffix".to_owned()));
                }
                let ports: Vec<_> = (start..end)
                    .filter(|port| port % modulus == suffix)
                    .map(|port| {
                        u16::try_from(port).map_err(|_| {
                            ConfigError::Semantic(format!(
                                "port range [{start}, {end}) contains an out-of-range port {port}"
                            ))
                        })
                    })
                    .collect::<Result<_, _>>()?;
                if ports.is_empty() {
                    return Err(ConfigError::Semantic(format!(
                        "port range [{start}, {end}) has no ports ending in {suffix}"
                    )));
                }
                Ok(ports)
            }
            (false, Some(ports)) => {
                if ports.is_empty() {
                    return Err(ConfigError::Semantic(
                        "listen_ports ports may not be empty".to_owned(),
                    ));
                }
                let unique: BTreeSet<_> = ports.iter().copied().collect();
                if unique.len() != ports.len() {
                    return Err(ConfigError::Semantic(
                        "listen_ports ports contains duplicates".to_owned(),
                    ));
                }
                Ok(unique.into_iter().collect())
            }
        }
    }
}

/// Validates that a literal remote target is usable for the address family.
///
/// # Errors
///
/// Returns [`ConfigError::Semantic`] when `address` does not match `family` or
/// is not an allowed unicast remote destination.
pub fn validate_target_address(
    family: AddressFamily,
    address: IpAddr,
    profile_name: &str,
) -> Result<(), ConfigError> {
    if !family.matches(address) {
        return Err(ConfigError::Semantic(format!(
            "profile {profile_name:?} remote target {address} does not match family"
        )));
    }
    let valid = match address {
        IpAddr::V4(ip) => {
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && ip.octets() != [255, 255, 255, 255]
        }
        IpAddr::V6(ip) => {
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_unicast_link_local()
        }
    };
    if valid {
        Ok(())
    } else {
        Err(ConfigError::Semantic(format!(
            "profile {profile_name:?} remote target {address} is not allowed"
        )))
    }
}

fn validate_source_cidrs(
    family: AddressFamily,
    configured: &[String],
    profile_name: &str,
) -> Result<Vec<String>, ConfigError> {
    let sources = if configured.is_empty() {
        vec![family.any_source().to_owned()]
    } else {
        configured.to_vec()
    };
    let mut deduplicated = BTreeSet::new();
    for source in &sources {
        let (address, bits) = source.rsplit_once('/').ok_or_else(|| {
            ConfigError::Semantic(format!(
                "profile {profile_name:?} invalid source CIDR {source:?}"
            ))
        })?;
        let address: IpAddr = address.parse().map_err(|_| {
            ConfigError::Semantic(format!(
                "profile {profile_name:?} invalid source CIDR {source:?}"
            ))
        })?;
        let max_bits = match address {
            IpAddr::V4(_) if family == AddressFamily::Ipv4 => 32,
            IpAddr::V6(_) if family == AddressFamily::Ipv6 => 128,
            _ => {
                return Err(ConfigError::Semantic(format!(
                    "profile {profile_name:?} source CIDR {source:?} does not match family"
                )));
            }
        };
        let bits: u8 = bits.parse().map_err(|_| {
            ConfigError::Semantic(format!(
                "profile {profile_name:?} invalid source CIDR {source:?}"
            ))
        })?;
        if bits > max_bits {
            return Err(ConfigError::Semantic(format!(
                "profile {profile_name:?} source CIDR {source:?} has an invalid prefix length"
            )));
        }
        if !deduplicated.insert(source.clone()) {
            return Err(ConfigError::Semantic(format!(
                "profile {profile_name:?} source_cidrs contains duplicates"
            )));
        }
    }
    Ok(sources)
}

/// Returns true for the IETF documentation-only address ranges. They are
/// useful in manuals, but a daemon must never claim that they are deployable
/// listener, source, or literal remote addresses.
#[must_use]
pub fn is_documentation_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => matches!(
            address.octets(),
            [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _]
        ),
        IpAddr::V6(address) => address.octets()[..4] == [0x20, 0x01, 0x0d, 0xb8],
    }
}

fn reject_placeholder(profile_name: &str, value: &str, field: &str) -> Result<(), ConfigError> {
    if value.trim().to_ascii_uppercase().contains("TODO") {
        Err(ConfigError::Semantic(format!(
            "profile {profile_name:?} field {field} still contains TODO value {value:?}; complete the first-use wizard or edit the field before start"
        )))
    } else {
        Ok(())
    }
}

fn decimal_width(value: u32) -> u32 {
    if value == 0 { 1 } else { value.ilog10() + 1 }
}

const fn family_name(family: AddressFamily) -> &'static str {
    match family {
        AddressFamily::Ipv4 => "ipv4",
        AddressFamily::Ipv6 => "ipv6",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(body: &str) -> Config {
        Config::from_toml(body).expect("valid config")
    }

    const REMOTE: &str = r#"
schema_version = 1
[[profiles]]
name = "web"
family = "ipv4"
listen_address = "192.0.2.10"
protocols = ["tcp", "udp"]
[profiles.listen_ports]
range_start = 20000
range_end = 65536
suffix = 443
[profiles.target]
kind = "remote"
host = "198.51.100.7"
port = 443
source_mode = "masquerade"
"#;

    #[test]
    fn parses_and_projects_tail_ports() {
        let profiles = config(REMOTE).validate().unwrap();
        assert_eq!(
            profiles[0].listen_ports,
            vec![
                20_443, 21_443, 22_443, 23_443, 24_443, 25_443, 26_443, 27_443, 28_443, 29_443,
                30_443, 31_443, 32_443, 33_443, 34_443, 35_443, 36_443, 37_443, 38_443, 39_443,
                40_443, 41_443, 42_443, 43_443, 44_443, 45_443, 46_443, 47_443, 48_443, 49_443,
                50_443, 51_443, 52_443, 53_443, 54_443, 55_443, 56_443, 57_443, 58_443, 59_443,
                60_443, 61_443, 62_443, 63_443, 64_443, 65_443
            ]
        );
    }

    #[test]
    fn accepts_end_exclusive_65536() {
        let ports = ListenPorts {
            range_start: Some(65_000),
            range_end: Some(65_536),
            suffix: Some(5),
            ports: None,
        }
        .project()
        .unwrap();
        assert_eq!(*ports.last().unwrap(), 65_535);
    }

    #[test]
    fn rejects_duplicate_listener_even_with_sources() {
        let duplicate = REMOTE.replace("name = \"web\"", "name = \"web2\"");
        let combined = format!(
            "{REMOTE}\n{}",
            duplicate.replace("schema_version = 1\n", "")
        );
        assert!(Config::from_toml(&combined).is_err());
    }

    #[test]
    fn rejects_ipv6_source_mode_and_loopback_without_opt_in() {
        let ipv6 = REMOTE
            .replace("family = \"ipv4\"", "family = \"ipv6\"")
            .replace("192.0.2.10", "2001:db8::10")
            .replace("198.51.100.7", "2001:db8::7");
        assert!(Config::from_toml(&ipv6).is_err());
        let loopback = REMOTE.replace(
            "kind = \"remote\"\nhost = \"198.51.100.7\"\nport = 443\nsource_mode = \"masquerade\"",
            "kind = \"loopback-dnat\"\nport = 443",
        );
        assert!(Config::from_toml(&loopback).is_err());
    }

    #[test]
    fn default_source_is_public_and_bad_targets_are_rejected() {
        let redirect = r#"
schema_version = 1
[[profiles]]
name = "dns"
family = "ipv6"
listen_address = "2001:db8::10"
protocols = ["udp"]
[profiles.listen_ports]
ports = [2053]
[profiles.target]
kind = "redirect"
port = 53
"#;
        let profile = config(redirect).validate().unwrap().remove(0);
        assert_eq!(profile.source_cidrs, ["::/0"]);
        assert!(
            validate_target_address(AddressFamily::Ipv4, "127.0.0.1".parse().unwrap(), "x")
                .is_err()
        );
        assert!(
            validate_target_address(AddressFamily::Ipv6, "fe80::1".parse().unwrap(), "x").is_err()
        );
    }

    #[test]
    fn external_chain_coexistence_is_opt_in() {
        assert!(!config("schema_version = 1").allow_external_chains);
        assert!(config("schema_version = 1\nallow_external_chains = true").allow_external_chains);
    }

    #[test]
    fn deployable_validation_rejects_documentation_addresses_and_todos() {
        let documentation = REMOTE;
        assert!(
            Config::from_toml(documentation)
                .unwrap()
                .validate_deployable()
                .unwrap_err()
                .to_string()
                .contains("documentation-reserved")
        );

        let todo = REMOTE
            .replace("192.0.2.10", "10.0.0.10")
            .replace("198.51.100.7", "10.0.0.7")
            .replace("name = \"web\"", "name = \"TODO-profile\"")
            .replace("198.51.100.0/24", "10.0.0.0/24");
        assert!(
            Config::from_toml(&todo)
                .unwrap()
                .validate_deployable()
                .unwrap_err()
                .to_string()
                .contains("TODO")
        );
    }
}
