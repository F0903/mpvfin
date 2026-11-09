//! The Docker Jellyfin, and a stand-in server that never answers.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use jellyfin::BaseUrl;
use serde_json::{Value, json};
use ureq::Agent;

use crate::{ADMIN, State, duration_from_ticks, header, login, text, ticks};

/// The Jellyfin test server, logged in as admin.
pub struct Server {
    pub(crate) base: BaseUrl,
    pub(crate) agent: Agent,
    auth: String,
    user_id: String,
}

impl Server {
    /// Logs in to `MPVFIN_E2E_SERVER`, default `http://127.0.0.1:8096`.
    pub fn connect() -> Self {
        let url = std::env::var("MPVFIN_E2E_SERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:8096".to_owned());
        let base = BaseUrl::parse(&url).expect("MPVFIN_E2E_SERVER");
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        let (user_id, token) = login(&agent, &base, "mpvfin-e2e-admin", ADMIN);
        let auth = header("mpvfin-e2e-admin", Some(&token));
        Self {
            base,
            agent,
            auth,
            user_id,
        }
    }

    /// The configured server URL, as `script-opts/mpvfin.conf` takes it.
    pub fn url(&self) -> &str {
        self.base.as_str()
    }

    pub(crate) fn get(&self, path: &str) -> Value {
        let url = self.base.join(path);
        self.agent
            .get(&url)
            .header("Authorization", &self.auth)
            .call()
            .and_then(|mut response| response.body_mut().read_json())
            .unwrap_or_else(|e| panic!("GET {url}: {e}"))
    }

    pub(crate) fn post(&self, path: &str, body: &Value) {
        let url = self.base.join(path);
        self.agent
            .post(&url)
            .header("Authorization", &self.auth)
            .send_json(body)
            .unwrap_or_else(|e| panic!("POST {url}: {e}"));
    }

    /// `(id, name)` of each episode, S01E01 first.
    pub fn episodes(&self) -> Vec<(String, String)> {
        let found = self.get("/Items?Recursive=true&IncludeItemTypes=Episode");
        let mut items = found["Items"].as_array().expect("Items").clone();
        items.sort_by_key(|item| item["IndexNumber"].as_u64());
        items
            .iter()
            .map(|item| (text(&item["Id"]), text(&item["Name"])))
            .collect()
    }

    /// Logs in as admin under `device_id`: the state of an already logged-in plugin.
    pub fn device_login(&self, device_id: &str) -> State {
        let (user_id, token) = login(&self.agent, &self.base, device_id, ADMIN);
        State {
            device_id: device_id.to_owned(),
            server: Some(self.base.as_str().to_owned()),
            user_id: Some(user_id),
            access_token: Some(token),
        }
    }

    /// The Jellyfin session of `device_id`, if any.
    pub fn mpvfin_session(&self, device_id: &str) -> Option<Value> {
        self.get(&format!("/Sessions?deviceId={device_id}"))
            .as_array()?
            .iter()
            .find(|session| session["DeviceId"] == device_id)
            .cloned()
    }

    /// Tells `session` to play `items` now.
    pub fn play(&self, session: &str, items: &[&str], start_index: usize, start: Duration) {
        self.post(
            &format!(
                "/Sessions/{session}/Playing?playCommand=PlayNow&itemIds={}&startIndex={start_index}&startPositionTicks={}",
                items.join(","),
                ticks(start),
            ),
            &json!({}),
        );
    }

    /// Tells `session` to play `item` now with a subtitle stream selected.
    pub fn play_with_subtitle(&self, session: &str, item: &str, subtitle_index: i64) {
        self.post(
            &format!(
                "/Sessions/{session}/Playing?playCommand=PlayNow&itemIds={item}&subtitleStreamIndex={subtitle_index}"
            ),
            &json!({}),
        );
    }

    /// Sends a playstate command, e.g. `Pause` or `Seek`.
    pub fn playstate(&self, session: &str, command: &str, seek: Option<Duration>) {
        let query = seek.map_or(String::new(), |d| {
            format!("?seekPositionTicks={}", ticks(d))
        });
        self.post(
            &format!("/Sessions/{session}/Playing/{command}{query}"),
            &json!({}),
        );
    }

    /// Sends a general command.
    pub fn command(&self, session: &str, name: &str, args: Value) {
        self.post(
            &format!("/Sessions/{session}/Command"),
            &json!({"Name": name, "Arguments": args}),
        );
    }

    /// Approves a Quick Connect code as admin.
    pub fn authorize_quick_connect(&self, code: &str) {
        self.post(&format!("/QuickConnect/Authorize?code={code}"), &json!({}));
    }

    /// Admin's saved resume position for the item.
    pub fn resume_position(&self, item_id: &str) -> Duration {
        let item = self.get(&format!("/Items/{item_id}?userId={}", self.user_id));
        duration_from_ticks(
            item["UserData"]["PlaybackPositionTicks"]
                .as_u64()
                .unwrap_or(0),
        )
    }

    /// Whether admin has `item_id` marked as played.
    pub fn is_played(&self, item_id: &str) -> bool {
        let item = self.get(&format!("/Items/{item_id}?userId={}", self.user_id));
        item["UserData"]["Played"] == true
    }

    /// Clears admin's resume position and played state, so stale ones cannot pass a test.
    pub fn clear_resume(&self, item_id: &str) {
        let url = self.base.join(&format!(
            "/UserPlayedItems/{item_id}?userId={}",
            self.user_id
        ));
        self.agent
            .delete(&url)
            .header("Authorization", &self.auth)
            .call()
            .unwrap_or_else(|e| panic!("DELETE {url}: {e}"));
    }

    /// The item's `MediaStreams`, from `PlaybackInfo`.
    pub fn media_streams(&self, item_id: &str) -> Vec<Value> {
        let info = self.get(&format!(
            "/Items/{item_id}/PlaybackInfo?userId={}",
            self.user_id
        ));
        info["MediaSources"][0]["MediaStreams"]
            .as_array()
            .expect("MediaStreams")
            .clone()
    }
}

/// A stand-in Jellyfin that accepts the plugin, casts one item over the
/// WebSocket and never answers `/Items/{id}`, so mpv's load hook stays held.
pub struct StallingServer {
    url: String,
    item_requested: Arc<AtomicBool>,
}

impl StallingServer {
    pub fn start(item: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
        let url = format!("http://{}", listener.local_addr().expect("stub address"));
        let item_requested = Arc::new(AtomicBool::new(false));
        let (item, requested) = (item.to_owned(), item_requested.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (item, requested) = (item.clone(), requested.clone());
                thread::spawn(move || stub_connection(stream, &item, &requested));
            }
        });
        Self {
            url,
            item_requested,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether the plugin has asked for the item, which it does with the hook held.
    pub fn item_requested(&self) -> bool {
        self.item_requested.load(Ordering::SeqCst)
    }
}

pub(crate) fn stub_connection(stream: TcpStream, item: &str, requested: &AtomicBool) {
    let mut start = [0; 16];
    let Ok(seen) = stream.peek(&mut start) else {
        return;
    };
    if start[..seen].starts_with(b"GET /socket") {
        let mut socket = tungstenite::accept(stream).expect("stub WebSocket handshake");
        let play =
            json!({"MessageType": "Play", "Data": {"ItemIds": [item], "PlayCommand": "PlayNow"}});
        socket
            .send(tungstenite::Message::text(play.to_string()))
            .expect("stub Play message");
        while socket.read().is_ok() {}
        return;
    }
    let mut reader = BufReader::new(&stream);
    let mut request = String::new();
    let _ = reader.read_line(&mut request);
    let mut body_length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            body_length = value.trim().parse().unwrap_or(0);
        }
    }
    let _ = reader.read_exact(&mut vec![0; body_length]);
    if request.contains(" /Items/") {
        requested.store(true, Ordering::SeqCst);
        loop {
            thread::park(); // keeps the connection open and unanswered
        }
    }
    let _ = (&stream).write_all(
        b"HTTP/1.1 204 No Content
Connection: close

",
    );
}
