//! A scripted second member of a watch-together group.

use std::collections::VecDeque;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use jellyfin::BaseUrl;
use serde_json::{Value, json};
use ureq::Agent;

use crate::server::Server;
use crate::{PARTNER, header, login, text, ticks};

/// A scripted second `SyncPlay` client: user `partner` on its own random device, with a
/// WebSocket reader thread. Each instance is a separate group member.
pub struct Partner {
    http: Arc<PartnerHttp>,
    inbox: Arc<Inbox>,
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
}

pub(crate) struct PartnerHttp {
    agent: Agent,
    base: BaseUrl,
    auth: String,
}

impl PartnerHttp {
    pub(crate) fn try_post(&self, path: &str, body: &Value) -> Result<(), String> {
        let url = self.base.join(path);
        self.agent
            .post(&url)
            .header("Authorization", &self.auth)
            .send_json(body)
            .map(drop)
            .map_err(|e| format!("POST {url}: {e}"))
    }

    pub(crate) fn post(&self, path: &str, body: &Value) {
        self.try_post(path, body).unwrap_or_else(|e| panic!("{e}"));
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

    /// Answers a load or seek like a well-behaved client: ping, then `Ready` at `position`.
    pub(crate) fn ready(&self, pid: &str, position: u64) -> Result<(), String> {
        self.try_post("/SyncPlay/Ping", &json!({"Ping": 10}))?;
        self.try_post(
            "/SyncPlay/Ready",
            &json!({"When": utc_now(), "PositionTicks": position, "IsPlaying": false, "PlaylistItemId": pid}),
        )
    }
}

pub(crate) struct Inbox {
    state: Mutex<InboxState>,
    changed: Condvar,
    auto_ready: AtomicBool,
}

impl Inbox {
    /// Locks the state; a poisoned lock (a test thread panicked) is still usable.
    pub(crate) fn lock(&self) -> MutexGuard<'_, InboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Default)]
pub(crate) struct InboxState {
    /// `(MessageType, Data)` not yet consumed by a `wait_*`.
    messages: VecDeque<(String, Value)>,
    queue: Value,
    /// The first reader or auto-`Ready` failure; `wait_*` and `Drop` panic with it.
    failure: Option<String>,
}

impl Partner {
    /// Logs in as `partner` on a fresh device, posts capabilities and opens the socket.
    pub fn connect(server: &Server) -> Self {
        let device = jellyfin::new_device_id();
        let (_, token) = login(&server.agent, &server.base, &device, PARTNER);
        let http = Arc::new(PartnerHttp {
            agent: server.agent.clone(),
            base: server.base.clone(),
            auth: header(&device, Some(&token)),
        });
        http.post(
            "/Sessions/Capabilities/Full",
            &json!({"PlayableMediaTypes": ["Video"], "SupportsMediaControl": true}),
        );
        let url = server
            .base
            .join(&format!("/socket?ApiKey={token}&deviceId={device}"))
            .replacen("http", "ws", 1);
        let (socket, _) = tungstenite::connect(url).expect("partner WebSocket");
        let inbox = Arc::new(Inbox {
            state: Mutex::new(InboxState::default()),
            changed: Condvar::new(),
            auto_ready: AtomicBool::new(true),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (http, inbox, stop) = (http.clone(), inbox.clone(), stop.clone());
            thread::spawn(move || partner_reader(socket, &http, &inbox, &stop))
        };
        Self {
            http,
            inbox,
            stop,
            reader: Some(reader),
        }
    }

    /// Whether item-changing `PlayQueue` and `Seek` are answered with `Ready` (default on).
    pub fn set_auto_ready(&self, on: bool) {
        self.inbox.auto_ready.store(on, Ordering::SeqCst);
    }

    /// Creates a group and waits for `GroupJoined`; returns the group id.
    pub fn create_group(&self, name: &str) -> String {
        self.http.post("/SyncPlay/New", &json!({"GroupName": name}));
        text(&self.wait_update("GroupJoined", Duration::from_secs(10))["GroupId"])
    }

    pub fn join(&self, group_id: &str) {
        self.http
            .post("/SyncPlay/Join", &json!({"GroupId": group_id}));
    }

    pub fn leave(&self) {
        self.http.post("/SyncPlay/Leave", &json!({}));
    }

    pub fn set_new_queue(&self, item_ids: &[&str], index: usize, start: Duration) {
        self.http.post(
            "/SyncPlay/SetNewQueue",
            &json!({"PlayingQueue": item_ids, "PlayingItemPosition": index, "StartPositionTicks": ticks(start)}),
        );
    }

    pub fn pause(&self) {
        self.http.post("/SyncPlay/Pause", &json!({}));
    }

    pub fn unpause(&self) {
        self.http.post("/SyncPlay/Unpause", &json!({}));
    }

    pub fn stop(&self) {
        self.http.post("/SyncPlay/Stop", &json!({}));
    }

    pub fn seek(&self, position: Duration) {
        self.http
            .post("/SyncPlay/Seek", &json!({"PositionTicks": ticks(position)}));
    }

    /// Reports the item loaded, for a test that has auto-`Ready` off.
    pub fn ready(&self, pid: &str, position: Duration) {
        let position = u64::try_from(ticks(position)).expect("ticks fit in u64");
        self.http
            .ready(pid, position)
            .unwrap_or_else(|e| panic!("{e}"));
    }

    /// `GET /SyncPlay/List`, as this user sees it.
    pub fn groups(&self) -> Vec<Value> {
        self.http
            .get("/SyncPlay/List")
            .as_array()
            .expect("group list")
            .clone()
    }

    /// The unconsumed `SyncPlayCommand`s, oldest first, without consuming them.
    pub fn commands(&self) -> Vec<Value> {
        self.inbox
            .lock()
            .messages
            .iter()
            .filter(|(kind, _)| kind == "SyncPlayCommand")
            .map(|(_, data)| data.clone())
            .collect()
    }

    /// Discards every message received so far. Call it before the action whose result you
    /// assert on, so an older message cannot satisfy a `wait_*`.
    pub fn clear(&self) {
        self.inbox.lock().messages.clear();
    }

    /// The oldest unconsumed `SyncPlayGroupUpdate` of `kind`: `{"GroupId","Data","Type"}`.
    /// Panics on a timeout or a reader failure.
    #[track_caller]
    pub fn wait_update(&self, kind: &str, timeout: Duration) -> Value {
        self.wait("SyncPlayGroupUpdate", "Type", kind, timeout)
    }

    /// The oldest unconsumed `SyncPlayCommand` whose `Command` is `kind`. Panics like
    /// `wait_update`.
    #[track_caller]
    pub fn wait_command(&self, kind: &str, timeout: Duration) -> Value {
        self.wait("SyncPlayCommand", "Command", kind, timeout)
    }

    /// The `Data` of the latest `PlayQueue` update, or null if none arrived.
    pub fn last_queue(&self) -> Value {
        self.inbox.lock().queue.clone()
    }

    #[track_caller]
    pub(crate) fn wait(&self, message: &str, field: &str, kind: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        let mut state = self.inbox.lock();
        loop {
            if let Some(failure) = state.failure.clone() {
                drop(state);
                panic!("partner failed: {failure}");
            }
            let found = state
                .messages
                .iter()
                .position(|(m, data)| m == message && data[field] == kind);
            if let Some(found) = found {
                return state.messages.remove(found).expect("found above").1;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(state); // panicking with the lock held would poison it
                panic!("no {message} {kind} within {timeout:?}");
            }
            state = self
                .inbox
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for Partner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if thread::panicking() {
            return;
        }
        let failure = self.inbox.lock().failure.take();
        if let Some(failure) = failure {
            panic!("partner failed: {failure}");
        }
    }
}

pub(crate) type PartnerSocket =
    tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

pub(crate) fn partner_reader(
    mut socket: PartnerSocket,
    http: &PartnerHttp,
    inbox: &Inbox,
    stop: &AtomicBool,
) {
    if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = socket.get_ref() {
        let _ = tcp.set_read_timeout(Some(Duration::from_millis(100)));
    }
    while !stop.load(Ordering::SeqCst) {
        match socket.read() {
            Ok(tungstenite::Message::Text(raw)) => {
                if let Ok(message) = serde_json::from_str::<Value>(raw.as_str()) {
                    partner_handle(&message, http, inbox);
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => {
                // An error while stopping comes from our own Drop, not from the server.
                if !stop.load(Ordering::SeqCst) {
                    partner_fail(inbox, format!("WebSocket: {e}"));
                }
                break;
            }
        }
    }
    let _ = socket.close(None);
    let _ = socket.flush();
}

pub(crate) fn partner_handle(message: &Value, http: &PartnerHttp, inbox: &Inbox) {
    let (kind, data) = (&message["MessageType"], message["Data"].clone());
    let mut ready = None;
    if kind == "SyncPlayGroupUpdate" && data["Type"] == "PlayQueue" {
        let queue = &data["Data"];
        inbox.lock().queue = queue.clone();
        let item_changing = ["NewPlaylist", "SetCurrentItem", "NextItem", "PreviousItem"]
            .iter()
            .any(|reason| queue["Reason"] == *reason);
        if let (true, Some(index)) = (item_changing, queue["PlayingItemIndex"].as_u64()) {
            let pid = &queue["Playlist"][usize::try_from(index).expect("index")]["PlaylistItemId"];
            ready = Some((text(pid), queue["StartPositionTicks"].as_u64().unwrap_or(0)));
        }
    } else if kind == "SyncPlayCommand" && data["Command"] == "Seek" {
        ready = Some((
            text(&data["PlaylistItemId"]),
            data["PositionTicks"].as_u64().unwrap_or(0),
        ));
    }
    if let Some(kind) = kind.as_str() {
        let mut state = inbox.lock();
        state.messages.push_back((kind.to_owned(), data));
        inbox.changed.notify_all();
    }
    let Some((pid, position)) = ready.filter(|_| inbox.auto_ready.load(Ordering::SeqCst)) else {
        return;
    };
    if let Err(e) = http.ready(&pid, position) {
        partner_fail(inbox, format!("auto-Ready: {e}"));
    }
}

/// Records the first failure and wakes any waiter.
pub(crate) fn partner_fail(inbox: &Inbox, failure: String) {
    inbox.lock().failure.get_or_insert(failure);
    inbox.changed.notify_all();
}

/// Now in UTC as `2026-11-08T20:09:51.398Z`, which is what `When` takes.
pub(crate) fn utc_now() -> String {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970");
    let (days, secs) = (since.as_secs() / 86400, since.as_secs() % 86400);
    // Civil date from a day count (Hinnant's algorithm).
    let z = i64::try_from(days).expect("days") + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60,
        since.subsec_millis()
    )
}
