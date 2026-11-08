//! The session supervisor (login, registration, WebSocket) and the job worker.
//!
//! Both run on their own threads; the caller's thread only queues jobs and
//! sets flags, so no network I/O or sleep ever runs on it.

use std::hash::{BuildHasher, Hasher, RandomState};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use std::{fmt, fs, io, thread};

use mpvfin_core::{Generation, PlaybackSnapshot, ReceiverEvent, ResolvedMedia, Secret};
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
use crate::state::{LockError, State, StateFile};
use crate::wire::{Inbound, parse_inbound};

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

/// Work for the job thread, run in submission order.
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
}

/// The running session; dropping it without `shutdown` detaches the threads.
pub struct SessionHandle {
    jobs: Sender<Job>,
    stop: Arc<AtomicBool>,
    worker_done: Receiver<()>,
    supervisor_done: Receiver<()>,
}

impl SessionHandle {
    /// Queues a job; never blocks.
    pub fn submit(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// Runs queued jobs for up to `flush`, then closes the WebSocket; returns
    /// within `flush` plus a second. Threads still stuck in a request are
    /// detached; they own everything they use.
    pub fn shutdown(self, flush: Duration) {
        let Self {
            jobs,
            stop,
            worker_done,
            supervisor_done,
        } = self;
        let started = Instant::now();
        drop(jobs); // the worker exits once the queue is empty
        let _ = worker_done.recv_timeout(flush);
        stop.store(true, Ordering::Relaxed); // also makes the worker drop what is left
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

/// Starts the supervisor and job threads and returns at once.
#[must_use]
pub fn start(paths: Paths, sink: Sink) -> SessionHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let creds = Arc::new(Mutex::new(None));
    let (jobs, queue) = mpsc::channel();
    let (supervisor_tx, supervisor_done) = mpsc::channel();
    let (worker_tx, worker_done) = mpsc::channel();
    let (s, st, c) = (sink.clone(), stop.clone(), creds.clone());
    spawn("jobs", &sink, move || {
        work(&queue, &s, &st, &c);
        drop(worker_tx); // also dropped on unwind: `recv_timeout` sees the thread end
    });
    let (s, st) = (sink.clone(), stop.clone());
    spawn("session", &s, move || {
        supervise(&paths, &sink, &st, &creds);
        drop(supervisor_tx);
    });
    SessionHandle {
        jobs,
        stop,
        worker_done,
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

fn set_creds(creds: &Mutex<Option<Creds>>, value: Option<Creds>) {
    *creds.lock().unwrap_or_else(PoisonError::into_inner) = value;
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

fn supervise(paths: &Paths, sink: &Sink, stop: &AtomicBool, creds: &Arc<Mutex<Option<Creds>>>) {
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
                    client,
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
            let connected = Instant::now();
            let Err(e) = listen(socket, sink, stop) else {
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

/// Starts the media proxy; it always sends the current login's header.
fn start_proxy(base: &BaseUrl, creds: &Arc<Mutex<Option<Creds>>>) -> io::Result<Proxy> {
    let creds = creds.clone();
    let auth: TokenSource = Arc::new(move || {
        let current = creds.lock().unwrap_or_else(PoisonError::into_inner);
        current.as_ref().map(|c| c.auth.clone())
    });
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
            "mpvfin: Quick Connect is disabled on this Jellyfin server",
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

/// Forwards inbound messages and sends keep-alives; `Ok` once `stop` is set.
fn listen(mut socket: Socket, sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let mut keepalive = Duration::from_secs(30);
    let mut sent = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = socket.close(None); // writes and flushes the close frame
            return Ok(());
        }
        if sent.elapsed() >= keepalive {
            socket
                .send(Message::text(r#"{"MessageType":"KeepAlive"}"#))
                .map_err(clean)?;
            sent = Instant::now();
        }
        match socket.read() {
            Ok(Message::Text(text)) => match parse_inbound(&text) {
                Inbound::Receiver(e) => sink(SessionEvent::Receiver(e)),
                Inbound::DisplayMessage { text, duration } => {
                    sink(SessionEvent::Osd { text, duration });
                }
                Inbound::ForceKeepAlive(interval) => keepalive = interval / 2,
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

fn work(queue: &Receiver<Job>, sink: &Sink, stop: &AtomicBool, creds: &Mutex<Option<Creds>>) {
    for job in queue {
        if stop.load(Ordering::Relaxed) {
            return; // the shutdown flush window is over
        }
        let current = creds.lock().unwrap_or_else(PoisonError::into_inner).clone();
        match job {
            Job::Resolve {
                generation,
                item_id,
            } => {
                // Always answered: an unanswered resolve would hold mpv's load hook forever.
                let result = current
                    .ok_or_else(|| "not connected to Jellyfin".to_owned())
                    .and_then(|c| resolve(&c, &item_id));
                let event = match result {
                    Ok(media) => ReceiverEvent::MediaResolved { generation, media },
                    Err(message) => ReceiverEvent::MediaResolutionFailed {
                        generation,
                        message,
                    },
                };
                sink(SessionEvent::Receiver(event));
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

fn resolve(c: &Creds, item_id: &str) -> Result<ResolvedMedia, String> {
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
    resolve_media(&c.media_base, item_id, &item, &info)
}

#[cfg(test)]
mod tests {
    use super::*;

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
