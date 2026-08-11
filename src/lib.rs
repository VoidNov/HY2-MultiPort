//! HY2-MultiPort core library.
//!
//! The binaries are deliberately thin: parsing and semantic checks, DNS state
//! selection, nft batch generation, and the on-disk state format are all kept
//! here so they can be unit tested without root privileges.

pub mod config;
pub mod control;
pub mod dns;
pub mod nft;
pub mod runtime;
pub mod state;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/port-forward/config.toml";
pub const DEFAULT_SOCKET_PATH: &str = "/run/port-forwardd.sock";
pub const DEFAULT_STATE_PATH: &str = "/var/lib/port-forward/state.json";
