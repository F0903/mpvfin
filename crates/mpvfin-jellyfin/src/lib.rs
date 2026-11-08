//! Jellyfin configuration, identity, URLs and the locked state file.

pub mod config;
pub mod identity;
pub mod report;
pub mod state;
pub mod wire;

pub use config::{BaseUrl, Config, parse_config};
pub use identity::{CLIENT, auth_header, device_name, media_auth_header, new_device_id};
pub use report::{progress_body, started_body, stopped_body};
pub use state::{LockError, State, StateFile};
pub use wire::{Inbound, duration_to_ticks, parse_inbound, ticks_to_duration};
