//! Jellyfin configuration, identity, URLs and the locked state file.

pub mod config;
pub mod identity;
pub mod state;

pub use config::{BaseUrl, Config, parse_config};
pub use identity::{CLIENT, auth_header, device_name, media_auth_header, new_device_id};
pub use state::{LockError, State, StateFile};
