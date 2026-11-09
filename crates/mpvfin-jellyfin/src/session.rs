//! The session supervisor (login, registration, WebSocket) and the job worker.
//!
//! All run on their own threads; the caller's thread only queues jobs and
//! sets flags, so no network I/O or sleep ever runs on it. `SyncPlay` requests
//! get a second job thread, so they never wait behind a report or a resolve.

use std::hash::{BuildHasher, Hasher, RandomState};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fmt, fs, io, thread};

use mpvfin_core::{
    Generation, GroupRequest, GroupSummary, PlaybackSnapshot, ReceiverEvent, ResolvedMedia, Secret,
};
use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::client::{Client, HttpError, reconnect_delay, sanitize};
use crate::config::{BaseUrl, parse_config};
use crate::identity::{auth_header, device_name, pct};
use crate::negotiate::{capabilities_body, playback_info_body, resolve_media};
use crate::proxy::{Proxy, TokenSource};
use crate::report::{progress_body, started_body, stopped_body};
use crate::segments::{parse_segments, segments_path};
use crate::state::{LockError, State, StateFile};
use crate::syncplay::{
    GroupUpdate, clock_sample, parse_groups, parse_utc_ms, request_path_and_body,
};
use crate::wire::{Inbound, parse_inbound};

/// `/GetUtcTime` samples per clock batch.
const CLOCK_SAMPLES: usize = 5;
/// Time between clock batches while in a group.
const CLOCK_PERIOD: Duration = Duration::from_mins(1);
/// Attempts in all for a request the group's wait depends on.
const GROUP_WAIT_ATTEMPTS: u32 = 3;
/// Between those attempts, so one network hiccup can't take all three.
const GROUP_RETRY_PAUSE: Duration = Duration::from_millis(500);

/// Where the plugin's files live.
#[derive(Debug)]
pub struct Paths {
    /// `script-opts/mpvfin.conf`.
    pub config_file: PathBuf,
    /// `script-opts/mpvfin-state.json`; its lock makes one window the cast target.
    pub state_file: PathBuf,
}

/// What the session asks of the plugin thread.
#[derive(Debug)]
pub enum SessionEvent {
    /// Feed to the reducer.
    Receiver(ReceiverEvent),
    /// Show on mpv's OSD.
    Osd {
        /// Message text.
        text: String,
        /// How long to show it.
        duration: Duration,
    },
    /// Write to mpv's log; the plugin adds the `[mpvfin] ` prefix.
    Log(String),
    /// The answer to [`Job::ListGroups`].
    GroupList(Vec<GroupSummary>),
    /// [`Job::ListGroups`] failed; the viewer has been told.
    GroupListFailed,
}

/// Receives session events from the session threads; must not block.
pub type Sink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// A playback lifecycle report.
#[derive(Debug)]
pub enum Report {
    /// `POST /Sessions/Playing`.
    Started(PlaybackSnapshot),
    /// `POST /Sessions/Playing/Progress`.
    Progress(PlaybackSnapshot),
    /// `POST /Sessions/Playing/Stopped`.
    Stopped {
        /// Final state.
        snapshot: PlaybackSnapshot,
        /// Playback ended in an error.
        failed: bool,
    },
}

/// Work for the job threads, each running its jobs in submission order.
#[derive(Debug)]
pub enum Job {
    /// Negotiate playback; answered with `MediaResolved` or `MediaResolutionFailed`.
    Resolve {
        /// Echoed in the answer.
        generation: Generation,
        /// Jellyfin item id.
        item_id: String,
    },
    /// Post a playback report.
    Report(Report),
    /// `POST /UserPlayedItems/{id}`; dropped when logged out.
    MarkPlayed {
        /// Jellyfin item id.
        item_id: String,
    },
    /// Send a `SyncPlay` request; runs on the group thread.
    Group(GroupRequest),
    /// `GET /SyncPlay/List`, answered with `GroupList`; runs on the group thread.
    ListGroups,
}

/// The running session; dropping it without `shutdown` detaches the threads.
pub struct SessionHandle {
    jobs: Sender<Job>,
    group_jobs: Sender<Job>,
    stop: Arc<AtomicBool>,
    worker_done: Receiver<()>,
    group_done: Receiver<()>,
    supervisor_done: Receiver<()>,
}

impl SessionHandle {
    /// Queues a job; never blocks.
    pub fn submit(&self, job: Job) {
        let queue = match job {
            Job::Group(_) | Job::ListGroups => &self.group_jobs,
            _ => &self.jobs,
        };
        let _ = queue.send(job);
    }

    /// Runs queued jobs for up to `flush`, then closes the WebSocket; returns
    /// within `flush` plus a second. Threads still stuck in a request are
    /// detached; they own everything they use.
    pub fn shutdown(self, flush: Duration) {
        let Self {
            jobs,
            group_jobs,
            stop,
            worker_done,
            group_done,
            supervisor_done,
        } = self;
        let started = Instant::now();
        // Both job threads exit once their queues are empty.
        drop(jobs);
        drop(group_jobs);
        let _ = worker_done.recv_timeout(flush);
        let _ = group_done.recv_timeout(flush.saturating_sub(started.elapsed()));
        stop.store(true, Ordering::Relaxed); // also makes the workers drop what is left
        // Unused flush time goes to the socket close (reads wake every 250 ms).
        let budget = flush.saturating_add(Duration::from_secs(1));
        let _ = supervisor_done.recv_timeout(budget.saturating_sub(started.elapsed()));
    }
}

/// Credentials the job worker uses; set by the supervisor after registration.
#[derive(Clone)]
struct Creds {
    client: Client,
    user_id: String,
    /// The `Authorization` value the media proxy sends upstream.
    auth: Secret,
    /// The media proxy; every URL given to mpv goes through it.
    media_base: BaseUrl,
}

/// The `SyncPlay` membership and clock, shared by the session threads.
#[derive(Default)]
struct GroupState {
    /// The group to rejoin after a reconnect.
    joined: Option<String>,
    /// The server has us in a group on the current socket.
    in_group: bool,
    /// Server clock minus local clock.
    offset_ms: i64,
}

/// Starts the supervisor, job, group and clock threads and returns at once.
#[must_use]
pub fn start(paths: Paths, sink: Sink) -> SessionHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let creds = Arc::new(Mutex::new(None));
    let group = Arc::new(Mutex::new(GroupState::default()));
    let (jobs, queue) = mpsc::channel();
    let (group_jobs, group_queue) = mpsc::channel();
    let (clock_wake, clock_woken) = mpsc::channel();
    let (supervisor_tx, supervisor_done) = mpsc::channel();
    let (worker_tx, worker_done) = mpsc::channel();
    let (group_tx, group_done) = mpsc::channel();
    let (s, st, c, g) = (sink.clone(), stop.clone(), creds.clone(), group.clone());
    spawn("jobs", &sink, move || {
        work(&queue, &s, &st, &c, &g);
        drop(worker_tx); // also dropped on unwind: `recv_timeout` sees the thread end
    });
    let (s, st, c, g) = (sink.clone(), stop.clone(), creds.clone(), group.clone());
    spawn("group", &sink, move || {
        work(&group_queue, &s, &st, &c, &g);
        drop(group_tx);
    });
    let (s, st, c, g) = (sink.clone(), stop.clone(), creds.clone(), group.clone());
    spawn("clock", &sink, move || {
        keep_clock(&clock_woken, &s, &st, &c, &g);
    });
    let (s, st) = (sink.clone(), stop.clone());
    spawn("session", &s, move || {
        supervise(&paths, &sink, &st, &creds, &group, &clock_wake);
        drop(supervisor_tx);
    });
    SessionHandle {
        jobs,
        group_jobs,
        stop,
        worker_done,
        group_done,
        supervisor_done,
    }
}

fn spawn(name: &str, sink: &Sink, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = thread::Builder::new()
        .name(format!("mpvfin-{name}"))
        .spawn(f)
    {
        log(sink, format!("cannot start the {name} thread: {e}"));
    }
}

fn log(sink: &Sink, text: String) {
    sink(SessionEvent::Log(text));
}

fn osd(sink: &Sink, text: &str, secs: u64) {
    sink(SessionEvent::Osd {
        text: text.to_owned(),
        duration: Duration::from_secs(secs),
    });
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn set_creds(creds: &Mutex<Option<Creds>>, value: Option<Creds>) {
    *locked(creds) = value;
}

/// Sleeps in 1 s slices; false as soon as `stop` is set.
fn sleep(stop: &AtomicBool, duration: Duration) -> bool {
    let end = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(Duration::from_secs(1)));
    }
    false
}

/// A uniform value in `[0, 1]` from the std hasher's random keys.
fn jitter() -> f64 {
    let bits = RandomState::new().build_hasher().finish() >> 32;
    f64::from(u32::try_from(bits).unwrap_or_default()) / f64::from(u32::MAX)
}

/// Logs `why`, then waits out the next reconnect delay; false once stopping.
fn backoff(attempt: &mut u32, why: &str, sink: &Sink, stop: &AtomicBool) -> bool {
    let delay = reconnect_delay(*attempt, jitter());
    *attempt = attempt.saturating_add(1);
    log(
        sink,
        format!("{why}; retrying in {:.1} s", delay.as_secs_f64()),
    );
    sleep(stop, delay)
}

fn save(file: &mut StateFile, state: &State, sink: &Sink) {
    if let Err(e) = file.save(state) {
        log(sink, format!("cannot save the state file: {e}"));
    }
}

fn supervise(
    paths: &Paths,
    sink: &Sink,
    stop: &AtomicBool,
    creds: &Arc<Mutex<Option<Creds>>>,
    group: &Mutex<GroupState>,
    clock_wake: &Sender<()>,
) {
    let config = fs::read_to_string(&paths.config_file)
        .map_err(|e| e.to_string())
        .and_then(|text| parse_config(&text));
    let Ok(config) = config else {
        log(
            sink,
            format!("no server configured in {}", paths.config_file.display()),
        );
        osd(sink, "mpvfin: set server=… in script-opts/mpvfin.conf", 5);
        return;
    };
    let base = config.server;
    let Some(mut file) = lock(&paths.state_file, sink, stop) else {
        return;
    };
    let mut state = file.load();
    if state.server.as_deref() != Some(base.as_str()) {
        state.access_token = None;
        state.user_id = None;
    }
    save(&mut file, &state, sink); // persists a freshly minted device id
    // Lives as long as this function; dropping it on any way out stops it.
    let proxy = match start_proxy(&base, creds) {
        Ok(proxy) => proxy,
        Err(e) => {
            log(sink, format!("cannot start the media proxy: {e}"));
            osd(sink, "mpvfin: cannot start the media proxy", 5);
            return;
        }
    };
    let name = device_name();
    let mut attempt = 0;
    let mut login_expired = false;
    'session: loop {
        // Each failure breaks out with one log line, then backs off.
        let why = 'attempt: {
            let saved = state.user_id.clone().zip(state.access_token.clone());
            let (user_id, token) = if let Some((user_id, token)) = saved {
                (user_id, Secret::new(token))
            } else {
                let anonymous = auth_header(&name, &state.device_id, None);
                let client = Client::new(base.clone(), anonymous);
                match quick_connect(&client, login_expired, sink, stop) {
                    Ok(Some((user_id, token))) => {
                        login_expired = false;
                        state.user_id = Some(user_id.clone());
                        state.access_token = Some(token.expose().to_owned());
                        state.server = Some(base.as_str().to_owned());
                        save(&mut file, &state, sink);
                        (user_id, token)
                    }
                    Ok(None) => return, // disabled, or stopping
                    Err(e) => break 'attempt format!("Quick Connect failed: {e}"),
                }
            };
            let auth = auth_header(&name, &state.device_id, Some(&token));
            let client = Client::new(base.clone(), auth.clone());
            match client.post("/Sessions/Capabilities/Full", &capabilities_body()) {
                Ok(_) => {}
                Err(HttpError::Unauthorized) => {
                    set_creds(creds, None);
                    state.access_token = None;
                    save(&mut file, &state, sink);
                    login_expired = true;
                    continue 'session;
                }
                Err(e) => break 'attempt format!("Jellyfin registration failed: {e}"),
            }
            let url = base.websocket(&token, &state.device_id);
            set_creds(
                creds,
                Some(Creds {
                    client: client.clone(),
                    user_id,
                    auth,
                    media_base: proxy.base().clone(),
                }),
            );
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let socket = match open(&url) {
                Ok(socket) => socket,
                Err(e) => break 'attempt format!("Jellyfin connection failed: {e}"),
            };
            log(sink, format!("connected to Jellyfin as {name}"));
            after_connect(&client, group, sink, clock_wake);
            let connected = Instant::now();
            let result = listen(socket, sink, stop, group, clock_wake);
            locked(group).in_group = false;
            let Err(e) = result else {
                return;
            };
            if connected.elapsed() >= Duration::from_mins(1) {
                attempt = 0;
            }
            format!("Jellyfin connection lost: {e}")
        };
        if !backoff(&mut attempt, &why, sink, stop) {
            return;
        }
    }
}

/// Starts a clock batch, so the offset exists before any command, and joins the
/// remembered group again: the old socket's close took us out of it. If that
/// fails the group is given up, so the reducer is not left in a ghost one.
fn after_connect(client: &Client, group: &Mutex<GroupState>, sink: &Sink, clock_wake: &Sender<()>) {
    let _ = clock_wake.send(());
    let remembered = locked(group).joined.clone();
    let Some(group_id) = remembered else {
        return;
    };
    let (path, body) = request_path_and_body(&GroupRequest::Join { group_id }, 0);
    if let Err(e) = client.post(path, &body) {
        log(sink, format!("rejoining the group failed: {e}"));
        locked(group).joined = None;
        sink(SessionEvent::Receiver(ReceiverEvent::GroupGone {
            message: "You are no longer in that group".to_owned(),
        }));
    }
}

/// Starts the media proxy; it always sends the current login's header.
fn start_proxy(base: &BaseUrl, creds: &Arc<Mutex<Option<Creds>>>) -> io::Result<Proxy> {
    let creds = creds.clone();
    let auth: TokenSource = Arc::new(move || locked(&creds).as_ref().map(|c| c.auth.clone()));
    Proxy::start(base.clone(), auth)
}

/// Takes the state file lock, waiting while another window holds it.
fn lock(path: &Path, sink: &Sink, stop: &AtomicBool) -> Option<StateFile> {
    let mut told = false;
    loop {
        match StateFile::lock(path) {
            Ok(file) => return Some(file),
            Err(LockError::Busy) => {
                if !std::mem::replace(&mut told, true) {
                    log(
                        sink,
                        "another mpv window is the Jellyfin cast target; waiting".into(),
                    );
                }
            }
            Err(LockError::Io(e)) => {
                log(sink, format!("cannot open {}: {e}", path.display()));
                return None;
            }
        }
        if !sleep(stop, Duration::from_secs(5)) {
            return None;
        }
    }
}

/// The Quick Connect OSD. Each refresh replaces the last message, so the
/// expired-login notice has to be part of the text.
fn quick_connect_text(code: &str, login_expired: bool) -> String {
    let usual = format!(
        "Jellyfin Quick Connect code: {code}\nApprove it in another Jellyfin app under Settings → Quick Connect"
    );
    if login_expired {
        format!("mpvfin: Jellyfin login expired\n{usual}")
    } else {
        usual
    }
}

/// Logs in with Quick Connect: `(user id, token)`, or `None` when the server
/// has it disabled or the session is stopping.
fn quick_connect(
    client: &Client,
    login_expired: bool,
    sink: &Sink,
    stop: &AtomicBool,
) -> Result<Option<(String, Secret)>, HttpError> {
    if client.get("/QuickConnect/Enabled")? != Value::Bool(true) {
        osd(
            sink,
            "mpvfin: turn on Quick Connect in the Jellyfin dashboard to sign in",
            5,
        );
        return Ok(None);
    }
    'initiate: loop {
        let reply = client.post("/QuickConnect/Initiate", &Value::Null)?;
        let field = |k: &str| reply.get(k).and_then(Value::as_str);
        let (Some(code), Some(secret)) = (field("Code"), field("Secret")) else {
            return Err(HttpError::transport("Quick Connect reply has no code"));
        };
        log(sink, format!("Quick Connect code: {code}"));
        let text = quick_connect_text(code, login_expired);
        let poll = format!("/QuickConnect/Connect?secret={}", pct(secret));
        for tick in 1u32.. {
            if tick % 5 == 1 {
                osd(sink, &text, 6);
            }
            if !sleep(stop, Duration::from_secs(1)) {
                return Ok(None);
            }
            if tick % 3 != 0 {
                continue;
            }
            match client.get(&poll) {
                Ok(status) if status.get("Authenticated") == Some(&Value::Bool(true)) => {
                    let login = client.post(
                        "/Users/AuthenticateWithQuickConnect",
                        &json!({ "Secret": secret }),
                    )?;
                    let user_id = login.pointer("/User/Id").and_then(Value::as_str);
                    let token = login.get("AccessToken").and_then(Value::as_str);
                    let (Some(user_id), Some(token)) = (user_id, token) else {
                        return Err(HttpError::transport(
                            "Quick Connect login reply has no token",
                        ));
                    };
                    return Ok(Some((user_id.to_owned(), Secret::new(token.to_owned()))));
                }
                Ok(_) => {}
                Err(HttpError::Status(404)) => continue 'initiate, // the secret expired
                Err(e) => return Err(e),
            }
        }
    }
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// Credential-free error text.
fn clean(e: impl fmt::Display) -> String {
    sanitize(&e.to_string())
}

/// Connects with a 5 s timeout per address and does the WebSocket handshake.
fn open(url: &Secret) -> Result<Socket, String> {
    let request = url.expose().into_client_request().map_err(clean)?;
    let uri = request.uri();
    let host = uri
        .host()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .to_owned();
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    let mut error = io::Error::new(io::ErrorKind::NotFound, "the server name has no address");
    let mut tcp = None;
    for addr in (host.as_str(), port).to_socket_addrs().map_err(clean)? {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(e) => error = e,
        }
    }
    let tcp = tcp.ok_or_else(|| clean(error))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(clean)?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(clean)?;
    let (socket, _) = tungstenite::client_tls(request, tcp).map_err(clean)?;
    // Set on the stream itself: on Windows a `try_clone` handle has its own timeouts.
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => tls.get_ref(),
        _ => return Err("unsupported WebSocket stream".into()),
    };
    // Short, because a quit with a stalled resolve only has the rest of its bound for this.
    tcp.set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(clean)?;
    Ok(socket)
}

fn local_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `When` (server time) minus the server's clock now.
fn command_delay_ms(when_ms: i64, local_now_ms: i64, offset_ms: i64) -> i64 {
    when_ms - (local_now_ms + offset_ms)
}

/// One `/GetUtcTime` round trip as `(offset, rtt)` in ms.
fn clock_probe(client: &Client) -> Result<(i64, i64), HttpError> {
    let t0 = local_now_ms();
    let reply = client.get("/GetUtcTime")?;
    let t3 = local_now_ms();
    let stamp = |key| {
        reply
            .get(key)
            .and_then(Value::as_str)
            .and_then(parse_utc_ms)
    };
    match (
        stamp("RequestReceptionTime"),
        stamp("ResponseTransmissionTime"),
    ) {
        (Some(t1), Some(t2)) => Ok(clock_sample(t0, t1, t2, t3)),
        _ => Err(HttpError::transport("bad /GetUtcTime reply")),
    }
}

/// Measures the clock when woken (at connect and `GroupJoined`), and every
/// `CLOCK_PERIOD` while in a group. Ends when the supervisor drops the wake sender.
fn keep_clock(
    woken: &Receiver<()>,
    sink: &Sink,
    stop: &AtomicBool,
    creds: &Mutex<Option<Creds>>,
    group: &Mutex<GroupState>,
) {
    loop {
        match woken.recv_timeout(CLOCK_PERIOD) {
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) if !locked(group).in_group => continue,
            _ => {}
        }
        let Some(c) = locked(creds).clone() else {
            continue;
        };
        let samples: Vec<_> = (0..CLOCK_SAMPLES)
            .take_while(|_| !stop.load(Ordering::Relaxed))
            .map(|_| clock_probe(&c.client))
            .collect();
        let Some(&(offset_ms, rtt)) = samples.iter().flatten().min_by_key(|(_, rtt)| *rtt) else {
            if let Some(Err(e)) = samples.last() {
                log(sink, format!("clock sync failed: {e}"));
            }
            continue;
        };
        let in_group = {
            let mut state = locked(group);
            state.offset_ms = offset_ms;
            state.in_group
        };
        // The server doubles this for its start delay, so it is one-way.
        if in_group
            && let Err(e) = c
                .client
                .post("/SyncPlay/Ping", &json!({ "Ping": (rtt / 2).max(0) }))
        {
            log(sink, format!("group ping failed: {e}"));
        }
    }
}

/// Tracks membership and passes one `SyncPlay` message to the reducer.
fn on_group_update(
    update: GroupUpdate,
    sink: &Sink,
    group: &Mutex<GroupState>,
    clock_wake: &Sender<()>,
) {
    if matches!(update, GroupUpdate::Left | GroupUpdate::Gone { .. }) {
        let mut state = locked(group);
        state.joined = None;
        state.in_group = false;
    }
    let event = match update {
        GroupUpdate::Joined { group_id } => {
            {
                let mut state = locked(group);
                state.joined = Some(group_id.clone());
                state.in_group = true;
            }
            let _ = clock_wake.send(());
            ReceiverEvent::GroupJoined { group_id }
        }
        GroupUpdate::Left => ReceiverEvent::GroupLeft,
        GroupUpdate::Gone { message } => ReceiverEvent::GroupGone {
            message: message.to_owned(),
        },
        GroupUpdate::Queue(queue) => ReceiverEvent::GroupQueueChanged(queue),
        GroupUpdate::Command {
            mut command,
            when_ms,
        } => {
            let offset_ms = locked(group).offset_ms;
            command.delay_ms = command_delay_ms(when_ms, local_now_ms(), offset_ms);
            ReceiverEvent::GroupCommandReceived(command)
        }
        GroupUpdate::Ignored => return,
    };
    sink(SessionEvent::Receiver(event));
}

/// Forwards inbound messages and sends keep-alives; `Ok` once `stop` is set.
fn listen(
    mut socket: Socket,
    sink: &Sink,
    stop: &AtomicBool,
    group: &Mutex<GroupState>,
    clock_wake: &Sender<()>,
) -> Result<(), String> {
    let mut keepalive = Duration::from_secs(30);
    let mut sent = Instant::now();
    let mut received = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = socket.close(None); // writes and flushes the close frame
            return Ok(());
        }
        // The server answers every keep-alive, so silence means a dead socket;
        // a send can go on succeeding into one for minutes.
        if received.elapsed() >= keepalive * 2 {
            return Err("nothing received for two keep-alive intervals".to_owned());
        }
        if sent.elapsed() >= keepalive {
            socket
                .send(Message::text(r#"{"MessageType":"KeepAlive"}"#))
                .map_err(clean)?;
            sent = Instant::now();
        }
        let read = socket.read();
        if read.is_ok() {
            received = Instant::now();
        }
        match read {
            Ok(Message::Text(text)) => match parse_inbound(&text) {
                Inbound::Receiver(e) => sink(SessionEvent::Receiver(e)),
                Inbound::DisplayMessage { text, duration } => {
                    sink(SessionEvent::Osd { text, duration });
                }
                Inbound::ForceKeepAlive(interval) => keepalive = interval / 2,
                Inbound::Group(update) => on_group_update(update, sink, group, clock_wake),
                Inbound::Ignored => {}
            },
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(clean(e)),
        }
    }
}

fn post_group_request(
    request: &GroupRequest,
    current: Option<&Creds>,
    group: &Mutex<GroupState>,
) -> Result<(), String> {
    let c = current.ok_or_else(|| "not connected".to_owned())?;
    // Stamped here, not at submit: this queue can be behind.
    let server_now_ms = local_now_ms() + locked(group).offset_ms;
    let (path, body) = request_path_and_body(request, server_now_ms);
    c.client.post(path, &body).map_err(|e| e.to_string())?;
    Ok(())
}

fn send_group_request(
    request: &GroupRequest,
    current: Option<&Creds>,
    sink: &Sink,
    stop: &AtomicBool,
    group: &Mutex<GroupState>,
) {
    if *request == GroupRequest::Leave {
        locked(group).joined = None; // even if the POST fails: never rejoin
    }
    // The server has no timeout: if one of these is lost, the group waits forever.
    let group_waits = matches!(
        request,
        GroupRequest::Ready { .. } | GroupRequest::Buffering { .. }
    );
    let mut result = post_group_request(request, current, group);
    for _ in 1..if group_waits { GROUP_WAIT_ATTEMPTS } else { 1 } {
        if result.is_ok() || !sleep(stop, GROUP_RETRY_PAUSE) {
            break;
        }
        result = post_group_request(request, current, group);
    }
    let Err(e) = result else {
        return;
    };
    log(sink, format!("group request failed: {e}"));
    if matches!(
        request,
        GroupRequest::New { .. } | GroupRequest::Join { .. }
    ) {
        osd(
            sink,
            "mpvfin: couldn't reach Jellyfin to join that group",
            5,
        );
    }
    if group_waits && !stop.load(Ordering::Relaxed) {
        give_up_on_group(current, sink, group);
    }
}

/// Out of the group, so it can go on without us; the viewer hears first, as
/// the Leave can stall as long as the requests that just failed.
fn give_up_on_group(current: Option<&Creds>, sink: &Sink, group: &Mutex<GroupState>) {
    {
        let mut state = locked(group);
        state.joined = None;
        state.in_group = false;
    }
    sink(SessionEvent::Receiver(ReceiverEvent::GroupGone {
        message: "mpvfin: lost contact with the group, so you've left it".to_owned(),
    }));
    if let Err(e) = post_group_request(&GroupRequest::Leave, current, group) {
        log(sink, format!("leaving the group failed: {e}"));
    }
}

/// Always answered, so the runtime's in-flight guard can't stick.
fn list_groups(current: Option<Creds>, sink: &Sink) {
    let list = current
        .ok_or_else(|| "not connected".to_owned())
        .and_then(|c| c.client.get("/SyncPlay/List").map_err(|e| e.to_string()));
    match list {
        Ok(list) => sink(SessionEvent::GroupList(parse_groups(&list))),
        Err(e) => {
            log(sink, format!("listing groups failed: {e}"));
            osd(sink, "mpvfin: couldn't reach Jellyfin to list groups", 5);
            sink(SessionEvent::GroupListFailed);
        }
    }
}

fn work(
    queue: &Receiver<Job>,
    sink: &Sink,
    stop: &AtomicBool,
    creds: &Mutex<Option<Creds>>,
    group: &Mutex<GroupState>,
) {
    for job in queue {
        if stop.load(Ordering::Relaxed) {
            return; // the shutdown flush window is over
        }
        let current = locked(creds).clone();
        match job {
            Job::Group(request) => {
                send_group_request(&request, current.as_ref(), sink, stop, group);
            }
            Job::ListGroups => list_groups(current, sink),
            Job::Resolve {
                generation,
                item_id,
            } => {
                // Always answered: an unanswered resolve would hold mpv's load hook forever.
                let result = current
                    .as_ref()
                    .ok_or_else(|| "not connected to Jellyfin".to_owned())
                    .and_then(|c| resolve(c, &item_id));
                let (event, runtime) = match result {
                    Ok((media, runtime)) => {
                        (ReceiverEvent::MediaResolved { generation, media }, runtime)
                    }
                    Err(message) => (
                        ReceiverEvent::MediaResolutionFailed {
                            generation,
                            message,
                        },
                        None,
                    ),
                };
                let resolved = matches!(event, ReceiverEvent::MediaResolved { .. });
                sink(SessionEvent::Receiver(event));
                // After MediaResolved, so playback never waits for segments; on
                // its own thread, so a stalled reply can't delay the next resolve.
                if let (true, Some(c)) = (resolved, current) {
                    let s = sink.clone();
                    spawn("segments", sink, move || {
                        match c.client.get(&segments_path(&item_id)) {
                            Ok(reply) => {
                                let segments = parse_segments(&reply, runtime);
                                s(SessionEvent::Receiver(ReceiverEvent::SegmentsResolved {
                                    generation,
                                    segments,
                                }));
                            }
                            Err(e) => log(&s, format!("media segments unavailable: {e}")),
                        }
                    });
                }
            }
            Job::MarkPlayed { item_id } => {
                let Some(c) = current else {
                    continue;
                };
                let path = format!(
                    "/UserPlayedItems/{}?userId={}",
                    pct(&item_id),
                    pct(&c.user_id)
                );
                if let Err(e) = c.client.post(&path, &Value::Null) {
                    log(sink, format!("marking the item played failed: {e}"));
                }
            }
            Job::Report(report) => {
                let Some(c) = current else {
                    continue; // no credentials: dropped
                };
                let (path, body) = match &report {
                    Report::Started(s) => ("/Sessions/Playing", started_body(s)),
                    Report::Progress(s) => ("/Sessions/Playing/Progress", progress_body(s)),
                    Report::Stopped { snapshot, failed } => {
                        ("/Sessions/Playing/Stopped", stopped_body(snapshot, *failed))
                    }
                };
                if let Err(e) = c.client.post(path, &body) {
                    log(sink, format!("playback report failed: {e}"));
                }
            }
        }
    }
}

/// The resolved media and the item's `RunTimeTicks`.
fn resolve(c: &Creds, item_id: &str) -> Result<(ResolvedMedia, Option<i64>), String> {
    let failed = |e: HttpError| format!("Jellyfin request failed: {e}");
    let id = pct(item_id);
    let item = c
        .client
        .get(&format!("/Items/{id}?userId={}", pct(&c.user_id)))
        .map_err(failed)?;
    let info = c
        .client
        .post(
            &format!("/Items/{id}/PlaybackInfo"),
            &playback_info_body(&c.user_id),
        )
        .map_err(failed)?;
    let runtime = item.get("RunTimeTicks").and_then(Value::as_i64);
    resolve_media(&c.media_base, item_id, &item, &info).map(|m| (m, runtime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::new_device_id;
    use crate::syncplay::format_utc;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;

    /// Answers everything except `/MediaSegments`, which it never answers and
    /// reports on `segments_asked`. Returns its URL.
    fn segments_stalling_server(segments_asked: Sender<()>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let asked = segments_asked.clone();
                thread::spawn(move || stub_connection(stream, &asked));
            }
        });
        url
    }

    fn is_websocket(stream: &TcpStream) -> bool {
        let mut start = [0; 7];
        stream.peek(&mut start).is_ok() && &start == b"GET /so"
    }

    /// The request line and body of the one request on `stream`.
    fn read_request(stream: &TcpStream) -> (String, String) {
        let mut reader = BufReader::new(stream);
        let (mut request, mut body_length) = (String::new(), 0);
        reader.read_line(&mut request).unwrap();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                body_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; body_length];
        reader.read_exact(&mut body).unwrap();
        (request, String::from_utf8(body).unwrap())
    }

    fn reply_json(mut stream: &TcpStream, status: &str, body: &str) {
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).unwrap();
    }

    fn stub_connection(stream: TcpStream, segments_asked: &Sender<()>) {
        if is_websocket(&stream) {
            let mut socket = tungstenite::accept(stream).unwrap();
            while socket.read().is_ok() {}
            return;
        }
        let (request, _) = read_request(&stream);
        if request.contains("/MediaSegments/") {
            segments_asked.send(()).unwrap();
            loop {
                thread::park(); // keeps the connection open and unanswered
            }
        }
        let body = if request.contains("/PlaybackInfo") {
            r#"{"PlaySessionId":"ps","MediaSources":[{"Id":"src","SupportsDirectPlay":true}]}"#
        } else {
            "{}"
        };
        reply_json(&stream, "200 OK", body);
    }

    /// Serves a group session. Each WebSocket connection sends what the test
    /// queues on `frames`, and `None` closes it. Every HTTP request is recorded
    /// on `seen` before it is answered; `/GetUtcTime` answers with the local
    /// clock plus `offset_ms`, and requests to a path containing `failing` fail.
    fn group_server(
        seen: Sender<(String, String)>,
        frames: Receiver<Option<String>>,
        offset_ms: i64,
        failing: Option<&'static str>,
    ) -> String {
        flaky_group_server(seen, frames, offset_ms, failing, usize::MAX)
    }

    /// As `group_server`, with only the first `failures` matching requests failing.
    fn flaky_group_server(
        seen: Sender<(String, String)>,
        frames: Receiver<Option<String>>,
        offset_ms: i64,
        failing: Option<&'static str>,
        failures: usize,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let frames = Arc::new(Mutex::new(frames));
        let failures = Arc::new(AtomicUsize::new(failures));
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (seen, frames, failures) = (seen.clone(), frames.clone(), failures.clone());
                thread::spawn(move || {
                    if is_websocket(&stream) {
                        let mut socket = tungstenite::accept(stream).unwrap();
                        let frames = frames.lock().unwrap();
                        while let Ok(Some(frame)) = frames.recv() {
                            socket.send(Message::text(frame)).unwrap();
                        }
                        drop(frames);
                        let _ = socket.close(None);
                        while socket.read().is_ok() {}
                        return;
                    }
                    let (request, body) = read_request(&stream);
                    let fails = failing.is_some_and(|path| request.contains(path))
                        && failures
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                            .is_ok();
                    let status = if fails {
                        "500 Internal Server Error"
                    } else {
                        "200 OK"
                    };
                    let reply = if request.contains("/GetUtcTime") {
                        let now = format_utc(local_now_ms() + offset_ms);
                        format!(
                            r#"{{"RequestReceptionTime":"{now}","ResponseTransmissionTime":"{now}"}}"#
                        )
                    } else {
                        "{}".to_owned()
                    };
                    let _ = seen.send((request, body));
                    reply_json(&stream, status, &reply);
                });
            }
        });
        url
    }

    /// Everything on `seen` up to and including the first request whose line
    /// contains `wanted`.
    fn requests_until(seen: &Receiver<(String, String)>, wanted: &str) -> Vec<(String, String)> {
        let mut requests = Vec::new();
        loop {
            let request = seen
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| {
                    let lines: Vec<_> = requests.iter().map(|(line, _)| line).collect();
                    panic!("no {wanted} request; saw {lines:?}")
                });
            let done = request.0.contains(wanted);
            requests.push(request);
            if done {
                return requests;
            }
        }
    }

    fn group_frame(kind: &str, group_id: &str) -> String {
        let data = json!({"GroupId": group_id, "Type": kind, "Data": {}});
        json!({"MessageType": "SyncPlayGroupUpdate", "Data": data}).to_string()
    }

    fn has_joined(event: &SessionEvent, wanted: &str) -> bool {
        matches!(
            event,
            SessionEvent::Receiver(ReceiverEvent::GroupJoined { group_id })
                if group_id == wanted
        )
    }

    /// Waits for an event `matches` accepts; false once `timeout` has passed.
    fn wait_for(
        events: &Receiver<SessionEvent>,
        timeout: Duration,
        matches: impl Fn(&SessionEvent) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while let Ok(event) =
            events.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if matches(&event) {
                return true;
            }
        }
        false
    }

    fn is_resolved(event: &SessionEvent, wanted: Generation) -> bool {
        matches!(
            event,
            SessionEvent::Receiver(ReceiverEvent::MediaResolved { generation, .. })
                if *generation == wanted
        )
    }

    fn resolve_job(generation: Generation, item_id: &str) -> Job {
        Job::Resolve {
            generation,
            item_id: item_id.to_owned(),
        }
    }

    /// A session logged in to the stub at `url`, already connected.
    fn connected_session(url: &str) -> (SessionHandle, Receiver<SessionEvent>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mpvfin-session-{}", new_device_id()));
        fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            config_file: dir.join("mpvfin.conf"),
            state_file: dir.join("mpvfin-state.json"),
        };
        fs::write(&paths.config_file, format!("server={url}\n")).unwrap();
        let state = State {
            device_id: "device".to_owned(),
            server: Some(BaseUrl::parse(url).unwrap().as_str().to_owned()),
            user_id: Some("user".to_owned()),
            access_token: Some("token".to_owned()),
        };
        fs::write(&paths.state_file, serde_json::to_vec(&state).unwrap()).unwrap();
        let (tx, events) = mpsc::channel();
        let session = start(
            paths,
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        );
        let connected =
            |e: &SessionEvent| matches!(e, SessionEvent::Log(l) if l.contains("connected"));
        assert!(wait_for(&events, Duration::from_secs(5), connected));
        (session, events, dir)
    }

    #[test]
    fn command_delay_uses_the_clock_offset() {
        // The server is 1000 ms ahead, so `When` 1500 is 300 ms away at local 200.
        assert_eq!(command_delay_ms(1500, 200, 1000), 300);
        // Already past: negative, so a late start can catch up.
        assert_eq!(command_delay_ms(1000, 200, 1000), -200);
    }

    #[test]
    fn reconnect_rejoins_the_remembered_group() {
        let (seen_tx, seen) = mpsc::channel();
        let (frames, frames_rx) = mpsc::channel();
        let url = group_server(seen_tx, frames_rx, 0, None);
        let (session, _events, dir) = connected_session(&url);

        frames.send(Some(group_frame("GroupJoined", "g1"))).unwrap();
        requests_until(&seen, "/SyncPlay/Ping");
        frames.send(None).unwrap(); // the socket drops while in the group

        // The rejoin must follow the repost, and nothing may Ping before the
        // server has us back: that would earn a NotInGroup and a false kick.
        requests_until(&seen, "/Sessions/Capabilities/Full");
        let mut requests = requests_until(&seen, "/SyncPlay/Join");
        let (_, body) = requests.last().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({"GroupId": "g1"})
        );
        requests.extend(std::iter::from_fn(|| {
            seen.recv_timeout(Duration::from_millis(300)).ok()
        }));
        assert!(
            !requests.iter().any(|(line, _)| line.contains("/Ping")),
            "pinged before the server had us back"
        );

        session.shutdown(Duration::ZERO);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn left_gone_and_a_requested_leave_prevent_a_rejoin() {
        enum Exit {
            Left,
            Gone,
            Requested,
        }
        for exit in [Exit::Requested, Exit::Left, Exit::Gone] {
            let (seen_tx, seen) = mpsc::channel();
            let (frames, frames_rx) = mpsc::channel();
            let url = group_server(seen_tx, frames_rx, 0, None);
            let (session, events, dir) = connected_session(&url);
            let soon = Duration::from_secs(5);

            frames.send(Some(group_frame("GroupJoined", "g1"))).unwrap();
            assert!(wait_for(&events, soon, |e| has_joined(e, "g1")));
            match exit {
                Exit::Left => {
                    frames.send(Some(group_frame("GroupLeft", "g1"))).unwrap();
                    assert!(wait_for(&events, soon, |e| matches!(
                        e,
                        SessionEvent::Receiver(ReceiverEvent::GroupLeft)
                    )));
                }
                Exit::Gone => {
                    frames.send(Some(group_frame("NotInGroup", "0"))).unwrap();
                    assert!(wait_for(&events, soon, |e| matches!(
                        e,
                        SessionEvent::Receiver(ReceiverEvent::GroupGone { .. })
                    )));
                }
                // No GroupLeft follows: the case of a Leave that never got through.
                Exit::Requested => {
                    session.submit(Job::Group(GroupRequest::Leave));
                    requests_until(&seen, "/SyncPlay/Leave");
                }
            }
            frames.send(None).unwrap();

            let connected =
                |e: &SessionEvent| matches!(e, SessionEvent::Log(l) if l.contains("connected"));
            assert!(wait_for(&events, soon, connected));
            // `listen` starts after any rejoin POST, so this ends the rejoin window.
            frames.send(Some(group_frame("GroupJoined", "g2"))).unwrap();
            assert!(wait_for(&events, soon, |e| has_joined(e, "g2")));
            let joins = seen
                .try_iter()
                .filter(|(line, _)| line.contains("/SyncPlay/Join"))
                .count();
            assert_eq!(joins, 0, "rejoined a group we had left");

            session.shutdown(Duration::ZERO);
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_failed_rejoin_reports_the_group_gone() {
        let (seen_tx, _seen) = mpsc::channel();
        let (frames, frames_rx) = mpsc::channel();
        let url = group_server(seen_tx, frames_rx, 0, Some("/SyncPlay/Join"));
        let (session, events, dir) = connected_session(&url);

        frames.send(Some(group_frame("GroupJoined", "g1"))).unwrap();
        frames.send(None).unwrap();
        assert!(
            wait_for(&events, Duration::from_secs(5), |e| matches!(
                e,
                SessionEvent::Receiver(ReceiverEvent::GroupGone { .. })
            )),
            "a failed rejoin must not leave the reducer in a ghost group"
        );

        session.shutdown(Duration::ZERO);
        let _ = fs::remove_dir_all(dir);
    }

    fn is_osd(event: &SessionEvent, wanted: &str) -> bool {
        matches!(event, SessionEvent::Osd { text, .. } if text == wanted)
    }

    fn is_list_failed(event: &SessionEvent) -> bool {
        matches!(event, SessionEvent::GroupListFailed)
    }

    const LIST_FAILED: &str = "mpvfin: couldn't reach Jellyfin to list groups";
    const JOIN_FAILED: &str = "mpvfin: couldn't reach Jellyfin to join that group";

    #[test]
    fn a_failed_group_list_tells_the_viewer_and_ends_the_request() {
        let (seen_tx, _seen) = mpsc::channel();
        let (_frames, frames_rx) = mpsc::channel();
        let url = group_server(seen_tx, frames_rx, 0, Some("/SyncPlay/List"));
        let (session, events, dir) = connected_session(&url);
        let soon = Duration::from_secs(5);

        session.submit(Job::ListGroups);
        assert!(wait_for(&events, soon, |e| is_osd(e, LIST_FAILED)));
        assert!(wait_for(&events, soon, is_list_failed));

        session.shutdown(Duration::ZERO);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn no_server_counts_as_a_failed_list_and_a_failed_join() {
        let dir = std::env::temp_dir().join(format!("mpvfin-session-{}", new_device_id()));
        let (tx, events) = mpsc::channel();
        let session = start(
            Paths {
                config_file: dir.join("mpvfin.conf"),
                state_file: dir.join("mpvfin-state.json"),
            },
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        );
        let soon = Duration::from_secs(5);

        session.submit(Job::ListGroups);
        assert!(wait_for(&events, soon, |e| is_osd(e, LIST_FAILED)));
        assert!(wait_for(&events, soon, is_list_failed));
        session.submit(Job::Group(GroupRequest::Join {
            group_id: "g".to_owned(),
        }));
        assert!(wait_for(&events, soon, |e| is_osd(e, JOIN_FAILED)));

        session.shutdown(Duration::ZERO);
    }

    #[test]
    fn only_a_failed_create_or_join_tells_the_viewer() {
        let (seen_tx, _seen) = mpsc::channel();
        let (_frames, frames_rx) = mpsc::channel();
        let url = group_server(seen_tx, frames_rx, 0, Some("/SyncPlay/"));
        let (session, events, dir) = connected_session(&url);
        let soon = Duration::from_secs(5);

        session.submit(Job::Group(GroupRequest::New {
            name: "n".to_owned(),
        }));
        assert!(wait_for(&events, soon, |e| is_osd(e, JOIN_FAILED)));
        session.submit(Job::Group(GroupRequest::Join {
            group_id: "g".to_owned(),
        }));
        assert!(wait_for(&events, soon, |e| is_osd(e, JOIN_FAILED)));

        session.submit(Job::Group(GroupRequest::Leave));
        assert!(wait_for(&events, soon, |e| {
            assert!(
                !matches!(e, SessionEvent::Osd { .. }),
                "only joining tells the viewer"
            );
            matches!(e, SessionEvent::Log(l) if l.contains("group request failed"))
        }));

        session.shutdown(Duration::ZERO);
        let _ = fs::remove_dir_all(dir);
    }

    fn ready_request() -> GroupRequest {
        GroupRequest::Ready {
            position: Duration::ZERO,
            is_playing: false,
            pid: "p".to_owned(),
        }
    }

    fn is_lost_contact(event: &SessionEvent) -> bool {
        matches!(
            event,
            SessionEvent::Receiver(ReceiverEvent::GroupGone { message })
                if message == "mpvfin: lost contact with the group, so you've left it"
        )
    }

    fn lines_with(requests: &[(String, String)], path: &str) -> usize {
        requests
            .iter()
            .filter(|(line, _)| line.contains(path))
            .count()
    }

    #[test]
    fn a_ready_that_fails_twice_is_sent_again_with_a_fresh_when() {
        let (seen_tx, seen) = mpsc::channel();
        let (_frames, frames_rx) = mpsc::channel();
        let url = flaky_group_server(seen_tx, frames_rx, 0, Some("/SyncPlay/Ready"), 2);
        let (session, events, dir) = connected_session(&url);

        session.submit(Job::Group(ready_request()));
        let stamps: Vec<i64> = (0..3)
            .map(|_| {
                let (_, body) = requests_until(&seen, "/SyncPlay/Ready").pop().unwrap();
                let body: Value = serde_json::from_str(&body).unwrap();
                parse_utc_ms(body["When"].as_str().unwrap()).unwrap()
            })
            .collect();
        assert!(stamps.is_sorted_by(|a, b| a < b), "stale When: {stamps:?}");

        let more: Vec<_> =
            std::iter::from_fn(|| seen.recv_timeout(Duration::from_secs(1)).ok()).collect();
        assert_eq!(
            lines_with(&more, "/SyncPlay/Ready"),
            0,
            "the third one worked"
        );
        assert_eq!(lines_with(&more, "/SyncPlay/Leave"), 0);
        assert!(!events.try_iter().any(|e| is_lost_contact(&e)));

        session.shutdown(Duration::ZERO);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_ready_or_buffering_that_keeps_failing_leaves_the_group() {
        let buffering = GroupRequest::Buffering {
            position: Duration::ZERO,
            pid: "p".to_owned(),
        };
        for (request, path) in [
            (ready_request(), "/SyncPlay/Ready"),
            (buffering, "/SyncPlay/Buffering"),
        ] {
            let (seen_tx, seen) = mpsc::channel();
            let (frames, frames_rx) = mpsc::channel();
            let url = group_server(seen_tx, frames_rx, 0, Some(path));
            let (session, events, dir) = connected_session(&url);
            let soon = Duration::from_secs(5);
            frames.send(Some(group_frame("GroupJoined", "g1"))).unwrap();
            assert!(wait_for(&events, soon, |e| has_joined(e, "g1")));

            session.submit(Job::Group(request));
            let requests = requests_until(&seen, "/SyncPlay/Leave");
            assert_eq!(lines_with(&requests, path), 3, "three attempts in all");
            assert!(wait_for(&events, soon, is_lost_contact), "{path}");

            // The group is given up, so a reconnect must not rejoin it.
            frames.send(None).unwrap();
            let connected =
                |e: &SessionEvent| matches!(e, SessionEvent::Log(l) if l.contains("connected"));
            assert!(wait_for(&events, soon, connected));
            frames.send(Some(group_frame("GroupJoined", "g2"))).unwrap();
            assert!(wait_for(&events, soon, |e| has_joined(e, "g2")));
            let later: Vec<_> = seen.try_iter().collect();
            assert_eq!(lines_with(&later, "/SyncPlay/Join"), 0);

            session.shutdown(Duration::ZERO);
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn other_failed_requests_are_sent_once_and_keep_the_group() {
        let (seen_tx, seen) = mpsc::channel();
        let (_frames, frames_rx) = mpsc::channel();
        let url = group_server(seen_tx, frames_rx, 0, Some("/SyncPlay/Unpause"));
        let (session, events, dir) = connected_session(&url);

        session.submit(Job::Group(GroupRequest::Unpause));
        requests_until(&seen, "/SyncPlay/Unpause");
        let more: Vec<_> =
            std::iter::from_fn(|| seen.recv_timeout(Duration::from_secs(1)).ok()).collect();
        assert_eq!(lines_with(&more, "/SyncPlay/"), 0, "{more:?}");
        assert!(!events.try_iter().any(|e| is_lost_contact(&e)));

        session.shutdown(Duration::ZERO);
        let _ = fs::remove_dir_all(dir);
    }

    /// A keep-alive interval of 500 ms, so two intervals of silence take 1 s.
    fn short_keep_alive() -> String {
        json!({"MessageType": "ForceKeepAlive", "Data": 1}).to_string()
    }

    fn is_connection_lost(event: &SessionEvent) -> bool {
        matches!(event, SessionEvent::Log(l) if l.contains("connection lost"))
    }

    #[test]
    fn a_socket_that_goes_silent_is_reconnected() {
        let (seen_tx, _seen) = mpsc::channel();
        let (frames, frames_rx) = mpsc::channel();
        let url = group_server(seen_tx, frames_rx, 0, None);
        let (session, events, dir) = connected_session(&url);

        frames.send(Some(short_keep_alive())).unwrap();
        assert!(
            wait_for(&events, Duration::from_secs(5), is_connection_lost),
            "the stub never answers a keep-alive, and nothing noticed"
        );

        session.shutdown(Duration::ZERO);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_socket_with_traffic_is_left_alone() {
        let (seen_tx, _seen) = mpsc::channel();
        let (frames, frames_rx) = mpsc::channel();
        let url = group_server(seen_tx, frames_rx, 0, None);
        let (session, events, dir) = connected_session(&url);

        frames.send(Some(short_keep_alive())).unwrap();
        for _ in 0..7 {
            thread::sleep(Duration::from_millis(300));
            let answer = json!({"MessageType": "KeepAlive"}).to_string();
            frames.send(Some(answer)).unwrap();
        }
        assert!(!events.try_iter().any(|e| is_connection_lost(&e)));

        session.shutdown(Duration::ZERO);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_clock_offset_times_commands_and_ready() {
        const OFFSET_MS: i64 = 3_600_000;
        let (seen_tx, seen) = mpsc::channel();
        let (frames, frames_rx) = mpsc::channel();
        let url = group_server(seen_tx, frames_rx, OFFSET_MS, None);
        let (session, events, dir) = connected_session(&url);

        // The batch at connect gives an offset before any group, and must not Ping.
        for _ in 0..CLOCK_SAMPLES {
            requests_until(&seen, "/GetUtcTime");
        }
        assert!(
            seen.recv_timeout(Duration::from_millis(200)).is_err(),
            "the connect batch sent more than its samples"
        );

        frames.send(Some(group_frame("GroupJoined", "g1"))).unwrap();
        let requests = requests_until(&seen, "/SyncPlay/Ping");
        let samples = requests
            .iter()
            .filter(|(line, _)| line.contains("/GetUtcTime"))
            .count();
        assert_eq!(samples, CLOCK_SAMPLES, "the Ping must follow the batch");
        let ping: Value = serde_json::from_str(&requests.last().unwrap().1).unwrap();
        assert!(ping["Ping"].is_i64(), "{ping}");

        let when = format_utc(local_now_ms() + OFFSET_MS + 2000);
        let command = json!({"MessageType": "SyncPlayCommand",
            "Data": {"GroupId": "g1", "PlaylistItemId": "p", "When": when,
                     "PositionTicks": 0, "Command": "Unpause"}});
        frames.send(Some(command.to_string())).unwrap();
        assert!(
            wait_for(&events, Duration::from_secs(5), |e| matches!(
                e,
                SessionEvent::Receiver(ReceiverEvent::GroupCommandReceived(c))
                    if (1000..=2100).contains(&c.delay_ms)
            )),
            "no command with a delay from the clock offset"
        );

        session.submit(Job::Group(GroupRequest::Ready {
            position: Duration::ZERO,
            is_playing: false,
            pid: "p".to_owned(),
        }));
        let (_, body) = requests_until(&seen, "/SyncPlay/Ready").pop().unwrap();
        let ready: Value = serde_json::from_str(&body).unwrap();
        let stamped = parse_utc_ms(ready["When"].as_str().unwrap()).unwrap();
        assert!(
            (stamped - (local_now_ms() + OFFSET_MS)).abs() < 5000,
            "Ready.When ignores the clock offset: {ready}"
        );

        session.shutdown(Duration::ZERO);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stalled_segments_call_does_not_hold_up_the_next_resolve() {
        let (asked_tx, segments_asked) = mpsc::channel();
        let url = segments_stalling_server(asked_tx);
        let (session, events, dir) = connected_session(&url);

        let (first, second) = (Generation::new(1).unwrap(), Generation::new(2).unwrap());
        session.submit(resolve_job(first, "a"));
        assert!(wait_for(&events, Duration::from_secs(5), |e| is_resolved(
            e, first
        )));
        segments_asked.recv_timeout(Duration::from_secs(5)).unwrap();

        // Well inside the plugin's 4 s resolve deadline.
        session.submit(resolve_job(second, "b"));
        assert!(
            wait_for(&events, Duration::from_secs(2), |e| is_resolved(e, second)),
            "the next resolve queued behind the stalled segments call"
        );

        let started = Instant::now();
        session.shutdown(Duration::ZERO);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "shutdown waited"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn expired_login_heads_the_quick_connect_osd() {
        let usual = quick_connect_text("123456", false);
        assert!(usual.starts_with("Jellyfin Quick Connect code: 123456\n"));
        assert_eq!(
            quick_connect_text("123456", true),
            format!("mpvfin: Jellyfin login expired\n{usual}")
        );
    }
}
