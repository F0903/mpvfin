//! The connection to Jellyfin: login, the socket, the job threads, and what
//! they keep on disk.

pub mod config;
pub mod proxy;
pub mod session;
pub mod state;

/// How mpvfin introduces itself to Jellyfin.
pub const APP: jellyfin::App = jellyfin::App {
    name: "mpvfin",
    version: env!("CARGO_PKG_VERSION"),
};
