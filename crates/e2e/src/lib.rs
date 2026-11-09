//! Harness for the end-to-end tests: a real Jellyfin (`Server`) and a real
//! mpv running the plugin (`Player`). Windows only (the IPC is a named pipe).
// Every helper panics on failure, by design; signatures follow the task brief.
#![allow(
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value
)]

use std::fs::{self};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use jellyfin::Secret;
use jellyfin::{App, BaseUrl, auth_header, new_device_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ureq::Agent;
mod partner;
mod player;
mod relay;
mod server;
pub use partner::Partner;
pub use player::{Player, PlayerOptions, write_config};
pub use relay::SocketRelay;
pub use server::{Server, StallingServer};

/// `mpvfin-state.json`, written out separately from the plugin's own type so
/// that a change to the format people already have on disk fails these tests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub device_id: String,
    pub server: Option<String>,
    pub user_id: Option<String>,
    pub access_token: Option<String>,
}

/// The harness's own identity toward the server.
const HARNESS: App = App {
    name: "mpvfin",
    version: env!("CARGO_PKG_VERSION"),
};

const ADMIN: (&str, &str) = ("admin", "mpvfin-test");
const PARTNER: (&str, &str) = ("partner", "mpvfin-test");

fn ticks(d: Duration) -> u128 {
    d.as_nanos() / 100
}

fn duration_from_ticks(ticks: u64) -> Duration {
    Duration::from_nanos(ticks.saturating_mul(100))
}

/// The session's reported `PlayState.PositionTicks`.
pub fn reported_position(session: &Value) -> Option<Duration> {
    session["PlayState"]["PositionTicks"]
        .as_u64()
        .map(duration_from_ticks)
}

fn header(device_id: &str, token: Option<&str>) -> String {
    let token = token.map(|t| Secret::new(t.to_owned()));
    auth_header(HARNESS, "mpvfin-e2e", device_id, token.as_ref())
        .expose()
        .to_owned()
}

fn login(
    agent: &Agent,
    base: &BaseUrl,
    device_id: &str,
    (user, password): (&str, &str),
) -> (String, String) {
    let url = base.join("/Users/AuthenticateByName");
    let login: Value = agent
        .post(&url)
        .header("Authorization", &header(device_id, None))
        .send_json(json!({"Username": user, "Pw": password}))
        .and_then(|mut response| response.body_mut().read_json())
        .unwrap_or_else(|e| panic!("POST {url}: {e}"));
    (text(&login["User"]["Id"]), text(&login["AccessToken"]))
}

fn text(value: &Value) -> String {
    value.as_str().expect("a JSON string").to_owned()
}

/// A temporary directory, removed on drop unless the test failed.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("mpvfin-e2e-{}", new_device_id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Default for TempDir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("kept for diagnosis: {}", self.0.display());
        } else {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

/// Polls `f` every 100 ms until it returns a value.
#[track_caller]
pub fn poll_until<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "condition not met within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}
