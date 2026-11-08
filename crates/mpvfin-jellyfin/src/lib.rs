//! Jellyfin configuration, identity, URLs, the locked state file and the
//! network session.

pub mod client;
pub mod config;
pub mod identity;
pub mod negotiate;
pub mod proxy;
pub mod report;
pub mod session;
pub mod state;
pub mod wire;

pub use config::{BaseUrl, Config, parse_config};
pub use identity::{CLIENT, auth_header, device_name, new_device_id};
pub use negotiate::{capabilities_body, display_title, playback_info_body, resolve_media};
pub use proxy::{Forward, Proxy, TokenSource, parse_request, response_head};
pub use report::{progress_body, started_body, stopped_body};
pub use session::{Job, Paths, Report, SessionEvent, SessionHandle, Sink, start};
pub use state::{LockError, State, StateFile};
pub use wire::{Inbound, duration_to_ticks, parse_inbound, ticks_to_duration};
