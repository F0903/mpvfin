//! Harness for the end-to-end tests: a real Jellyfin (`Server`) and a real
//! mpv running the plugin (`Player`). Windows only (the IPC is a named pipe).
// Every helper panics on failure, by design; signatures follow the task brief.
#![allow(
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value
)]

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use mpvfin_core::Secret;
pub use mpvfin_jellyfin::State;
use mpvfin_jellyfin::{BaseUrl, auth_header, new_device_id};
use serde_json::{Value, json};
use ureq::Agent;

const ADMIN: (&str, &str) = ("admin", "mpvfin-test");
const IPC_TIMEOUT: Duration = Duration::from_secs(10);
const QUIT_TIMEOUT: Duration = Duration::from_secs(30);

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

/// The Jellyfin test server, logged in as admin.
pub struct Server {
    base: BaseUrl,
    agent: Agent,
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
        let (user_id, token) = login(&agent, &base, "mpvfin-e2e-admin");
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

    fn get(&self, path: &str) -> Value {
        let url = self.base.join(path);
        self.agent
            .get(&url)
            .header("Authorization", &self.auth)
            .call()
            .and_then(|mut response| response.body_mut().read_json())
            .unwrap_or_else(|e| panic!("GET {url}: {e}"))
    }

    fn post(&self, path: &str, body: &Value) {
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
        let (user_id, token) = login(&self.agent, &self.base, device_id);
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

    /// Clears admin's resume position, so a stale one cannot pass a test.
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

fn stub_connection(stream: TcpStream, item: &str, requested: &AtomicBool) {
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

fn header(device_id: &str, token: Option<&str>) -> String {
    let token = token.map(|t| Secret::new(t.to_owned()));
    auth_header("mpvfin-e2e", device_id, token.as_ref())
        .expose()
        .to_owned()
}

fn login(agent: &Agent, base: &BaseUrl, device_id: &str) -> (String, String) {
    let url = base.join("/Users/AuthenticateByName");
    let login: Value = agent
        .post(&url)
        .header("Authorization", &header(device_id, None))
        .send_json(json!({"Username": ADMIN.0, "Pw": ADMIN.1}))
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

pub struct PlayerOptions {
    pub server: String,
    pub state: State,
    pub mpv_conf: String,
    /// `None`: a temporary directory that lives as long as the `Player`.
    /// Otherwise the caller's; config files already there are kept.
    pub config_dir: Option<PathBuf>,
}

/// Writes the config files `Player::launch` needs into `dir`.
pub fn write_config(dir: &Path, opts: &PlayerOptions) {
    let scripts = dir.join("script-opts");
    fs::create_dir_all(&scripts).expect("create script-opts");
    let write = |path: PathBuf, content: &[u8]| {
        fs::write(&path, content).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    };
    let conf = format!("{}# e2e-sentinel\n", opts.mpv_conf);
    write(dir.join("mpv.conf"), conf.as_bytes());
    write(dir.join("input.conf"), b"# e2e\n");
    write(
        scripts.join("mpvfin.conf"),
        format!("server={}\n", opts.server).as_bytes(),
    );
    write(
        scripts.join("mpvfin-state.json"),
        &serde_json::to_vec(&opts.state).expect("serialize state"),
    );
}

type IpcRequest = (u64, String, Sender<Value>);

/// A spawned mpv with the plugin loaded and a private config dir.
pub struct Player {
    child: Child,
    requests: Option<Sender<IpcRequest>>,
    next_request: AtomicU64,
    config_dir: PathBuf,
    log_path: PathBuf,
    _owned_dir: Option<TempDir>,
}

impl Player {
    pub fn launch(opts: PlayerOptions) -> Self {
        let owned_dir = opts.config_dir.is_none().then(TempDir::new);
        let config_dir = match (&opts.config_dir, &owned_dir) {
            (Some(dir), _) => dir.clone(),
            (None, owned) => owned.as_ref().expect("just created").path().to_owned(),
        };
        // A second player on a shared dir must not touch the first's locked state.
        if !config_dir.join("mpv.conf").exists() {
            write_config(&config_dir, &opts);
        }
        let unique = new_device_id();
        let log_path = PathBuf::from(format!("{}.{unique}.log", config_dir.display()));
        let pipe = format!(r"\\.\pipe\mpvfin-e2e-{unique}");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mpv = std::env::var_os("MPVFIN_E2E_MPV")
            .map_or_else(|| root.join(".tools/mpv/mpv.com"), PathBuf::from);
        let mut child = Command::new(&mpv)
            .arg(format!("--config-dir={}", config_dir.display()))
            .args(["--idle=yes", "--vo=null", "--ao=null"])
            .arg(format!(
                "--script={}",
                root.join("target/debug/mpvfin.dll").display()
            ))
            .arg(format!("--input-ipc-server={pipe}"))
            .arg(format!("--log-file={}", log_path.display()))
            .arg("--msg-level=all=debug")
            // A stray Enter or media key on this machine would skip to the next item.
            .arg("--input-default-bindings=no")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", mpv.display()));
        let Some(file) = connect_pipe(&pipe) else {
            kill_tree(&mut child);
            panic!("mpv did not create {pipe}");
        };
        let (requests, incoming) = mpsc::channel();
        thread::spawn(move || ipc_worker(&file, &incoming));
        Self {
            child,
            requests: Some(requests),
            next_request: AtomicU64::new(1),
            config_dir,
            log_path,
            _owned_dir: owned_dir,
        }
    }

    fn send(&self, command: &[Value]) -> mpsc::Receiver<Value> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let line = format!("{}\n", json!({"command": command, "request_id": id}));
        let (reply, response) = mpsc::channel();
        let requests = self.requests.as_ref().expect("live until drop");
        requests.send((id, line, reply)).expect("mpv IPC gone");
        response
    }

    fn request(&self, command: &[Value]) -> Value {
        self.send(command)
            .recv_timeout(IPC_TIMEOUT)
            .unwrap_or_else(|_| panic!("no mpv IPC reply to {command:?} in {IPC_TIMEOUT:?}"))
    }

    /// Reads a property; `Value::Null` while it is unavailable.
    pub fn get(&self, property: &str) -> Value {
        let mut reply = self.request(&[json!("get_property"), json!(property)]);
        match reply["error"].as_str() {
            Some("success") => reply["data"].take(),
            Some("property unavailable") => Value::Null,
            other => panic!("get {property}: {other:?}"),
        }
    }

    /// Runs an mpv command and requires it to succeed.
    pub fn command(&self, args: &[&str]) {
        let command: Vec<Value> = args.iter().map(|arg| json!(arg)).collect();
        let reply = self.request(&command);
        assert_eq!(reply["error"], "success", "{args:?}");
    }

    /// Waits for a log line containing `needle` and returns it.
    #[track_caller]
    pub fn wait_log(&self, needle: &str, timeout: Duration) -> String {
        poll_until(timeout, || {
            self.log_text()
                .lines()
                .find(|line| line.contains(needle))
                .map(str::to_owned)
        })
    }

    /// Asks mpv to quit; returns the time until the process exited.
    pub fn quit(self) -> Duration {
        self.quit_with_log().0
    }

    /// `quit`, plus the whole log, shutdown lines included.
    pub fn quit_with_log(self) -> (Duration, String) {
        self.quit_using("quit")
    }

    /// `quit_with_log`, but through `quit-watch-later`.
    pub fn quit_watch_later(self) -> (Duration, String) {
        self.quit_using("quit-watch-later")
    }

    fn quit_using(mut self, command: &str) -> (Duration, String) {
        let started = Instant::now();
        // mpv may close the pipe before replying, so the reply is not awaited.
        drop(self.send(&[json!(command)]));
        while Instant::now() < started + QUIT_TIMEOUT {
            if self.child.try_wait().ok().flatten().is_some() {
                return (started.elapsed(), self.log_text());
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("mpv did not exit within {QUIT_TIMEOUT:?}");
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn log_text(&self) -> String {
        fs::read(&self.log_path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.requests = None;
        kill_tree(&mut self.child);
        if thread::panicking() {
            eprintln!("kept for diagnosis: {}", self.log_path.display());
        } else {
            let _ = fs::remove_file(&self.log_path);
        }
    }
}

/// Kills `child` if it is still running, with its whole process tree:
/// `mpv.com` is a wrapper, and killing it alone would orphan `mpv.exe`.
fn kill_tree(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn connect_pipe(pipe: &str) -> Option<File> {
    let deadline = Instant::now() + IPC_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(file) = OpenOptions::new().read(true).write(true).open(pipe) {
            return Some(file);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Owns the pipe. Requests run one at a time, because a synchronous pipe
/// handle cannot read and write concurrently. Ends when mpv closes the pipe.
fn ipc_worker(pipe: &File, requests: &mpsc::Receiver<IpcRequest>) {
    let mut writer = pipe;
    let mut reader = BufReader::new(pipe);
    for (id, line, reply) in requests {
        if writer.write_all(line.as_bytes()).is_err() {
            return;
        }
        loop {
            let mut text = String::new();
            match reader.read_line(&mut text) {
                Ok(1..) => {}
                _ => return,
            }
            // Other lines are mpv events.
            if let Ok(value) = serde_json::from_str::<Value>(&text)
                && value["request_id"] == id
            {
                let _ = reply.send(value);
                break;
            }
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
