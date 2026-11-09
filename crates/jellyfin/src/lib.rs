//! A Jellyfin client: typed requests, the WebSocket, and the protocol's types.
//! It knows nothing about the player that uses it.

mod client;
mod identity;
mod negotiate;
mod quick_connect;
mod report;
mod segments;
mod socket;
mod syncplay;
mod thumbnail;
mod url;
mod wire;

pub use client::{Client, HttpError, reconnect_delay, tls_config};
pub use identity::{App, Secret, auth_header, device_name, new_device_id};
pub use negotiate::{PlayMethod, ResolvedMedia, TrackMap};
pub use quick_connect::QuickConnectCode;
pub use report::PlaybackProgress;
pub use segments::{Segment, SegmentKind};
pub use socket::Socket;
pub use syncplay::{
    GroupCommand, GroupCommandKind, GroupEntry, GroupQueue, GroupRequest, GroupSummary,
    GroupUpdate, QueueReason, ZERO_ID, format_utc, local_now_ms, parse_utc_ms,
};
pub use url::BaseUrl;
pub use wire::{Inbound, QueueMode, QueueRequest, RemoteCommand, StreamSelection};
