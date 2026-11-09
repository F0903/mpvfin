//! A Jellyfin client: typed requests, the WebSocket, and the protocol's types.
//! It knows nothing about the player that uses it.

mod client;
mod identity;
mod playback;
mod quick_connect;
mod remote;
mod socket;
mod syncplay;
mod time;
mod url;

pub use client::{Client, HttpError, reconnect_delay, tls_config};
pub use identity::{App, Secret, auth_header, device_name, new_device_id};
pub use playback::{PlayMethod, PlaybackProgress, ResolvedMedia, Segment, SegmentKind, TrackMap};
pub use quick_connect::QuickConnectCode;
pub use remote::{Inbound, QueueMode, QueueRequest, RemoteCommand, StreamSelection};
pub use socket::Socket;
pub use syncplay::{
    GroupCommand, GroupCommandKind, GroupEntry, GroupQueue, GroupRequest, GroupSummary,
    GroupUpdate, QueueReason, ZERO_ID, local_now_ms,
};
pub use time::{format_utc, parse_utc_ms};
pub use url::BaseUrl;
