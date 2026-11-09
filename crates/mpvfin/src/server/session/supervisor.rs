//! The session thread: the state file, logging in, registering, and keeping
//! the WebSocket open.

use std::hash::{BuildHasher, Hasher, RandomState};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fs, io};

use jellyfin::{
    BaseUrl, Client, GroupRequest, HttpError, Inbound, QuickConnectCode, Secret, Socket,
    auth_header, device_name, reconnect_delay,
};

use crate::receiver::ReceiverEvent;
use crate::server::APP;
use crate::server::config::parse_config;
use crate::server::proxy::{Proxy, TokenSource};
use crate::server::session::group::on_group_update;
use crate::server::session::{
    Creds, GroupState, Paths, SessionEvent, Sink, locked, log, osd, set_creds, sleep,
};
use crate::server::state::{LockError, State, StateFile};

/// A uniform value in `[0, 1]` from the std hasher's random keys.
pub(super) fn jitter() -> f64 {
    let bits = RandomState::new().build_hasher().finish() >> 32;
    f64::from(u32::try_from(bits).unwrap_or_default()) / f64::from(u32::MAX)
}

/// Logs `why`, then waits out the next reconnect delay; false once stopping.
pub(super) fn backoff(attempt: &mut u32, why: &str, sink: &Sink, stop: &AtomicBool) -> bool {
    let delay = reconnect_delay(*attempt, jitter());
    *attempt = attempt.saturating_add(1);
    log(
        sink,
        format!("{why}; retrying in {:.1} s", delay.as_secs_f64()),
    );
    sleep(stop, delay)
}

pub(super) fn save(file: &mut StateFile, state: &State, sink: &Sink) {
    if let Err(e) = file.save(state) {
        log(sink, format!("cannot save the state file: {e}"));
    }
}

pub(super) fn supervise(
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
    let name = device_name(APP.name);
    let mut attempt = 0;
    let mut login_expired = false;
    'session: loop {
        // Each failure breaks out with one log line, then backs off.
        let why = 'attempt: {
            let saved = state.user_id.clone().zip(state.access_token.clone());
            let (user_id, token) = if let Some((user_id, token)) = saved {
                (user_id, Secret::new(token))
            } else {
                let anonymous = auth_header(APP, &name, &state.device_id, None);
                let client = Client::new(base.clone(), APP, anonymous);
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
            let auth = auth_header(APP, &name, &state.device_id, Some(&token));
            let client = Client::new(base.clone(), APP, auth.clone());
            match client.report_capabilities() {
                Ok(()) => {}
                Err(HttpError::Unauthorized) => {
                    set_creds(creds, None);
                    state.access_token = None;
                    save(&mut file, &state, sink);
                    login_expired = true;
                    continue 'session;
                }
                Err(e) => break 'attempt format!("Jellyfin registration failed: {e}"),
            }
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
            let socket = match Socket::connect(&base, &token, &state.device_id) {
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
pub(super) fn after_connect(
    client: &Client,
    group: &Mutex<GroupState>,
    sink: &Sink,
    clock_wake: &Sender<()>,
) {
    let _ = clock_wake.send(());
    let remembered = locked(group).joined.clone();
    let Some(group_id) = remembered else {
        return;
    };
    if let Err(e) = client.syncplay(&GroupRequest::Join { group_id }, 0) {
        log(sink, format!("rejoining the group failed: {e}"));
        locked(group).joined = None;
        sink(SessionEvent::Receiver(ReceiverEvent::GroupGone {
            message: "You are no longer in that group".to_owned(),
        }));
    }
}

/// Starts the media proxy; it always sends the current login's header.
pub(super) fn start_proxy(base: &BaseUrl, creds: &Arc<Mutex<Option<Creds>>>) -> io::Result<Proxy> {
    let creds = creds.clone();
    let auth: TokenSource = Arc::new(move || locked(&creds).as_ref().map(|c| c.auth.clone()));
    Proxy::start(base.clone(), auth)
}

/// Takes the state file lock, waiting while another window holds it.
pub(super) fn lock(path: &Path, sink: &Sink, stop: &AtomicBool) -> Option<StateFile> {
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
pub(super) fn quick_connect_text(code: &str, login_expired: bool) -> String {
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
pub(super) fn quick_connect(
    client: &Client,
    login_expired: bool,
    sink: &Sink,
    stop: &AtomicBool,
) -> Result<Option<(String, Secret)>, HttpError> {
    if !client.quick_connect_enabled()? {
        osd(
            sink,
            "mpvfin: turn on Quick Connect in the Jellyfin dashboard to sign in",
            5,
        );
        return Ok(None);
    }
    'initiate: loop {
        let QuickConnectCode { code, secret } = client.quick_connect_initiate()?;
        log(sink, format!("Quick Connect code: {code}"));
        let text = quick_connect_text(&code, login_expired);
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
            match client.quick_connect_approved(&secret) {
                Ok(true) => return client.quick_connect_login(&secret).map(Some),
                Ok(false) => {}
                Err(HttpError::Status(404)) => continue 'initiate, // the secret expired
                Err(e) => return Err(e),
            }
        }
    }
}

/// Forwards inbound messages and sends keep-alives; `Ok` once `stop` is set.
pub(super) fn listen(
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
            socket.close();
            return Ok(());
        }
        // The server answers every keep-alive, so silence means a dead socket;
        // a send can go on succeeding into one for minutes.
        if received.elapsed() >= keepalive * 2 {
            return Err("nothing received for two keep-alive intervals".to_owned());
        }
        if sent.elapsed() >= keepalive {
            socket.send_keep_alive()?;
            sent = Instant::now();
        }
        let Some(inbound) = socket.read()? else {
            continue;
        };
        received = Instant::now();
        match inbound {
            Inbound::Command(c) => sink(SessionEvent::Receiver(c.into())),
            Inbound::DisplayMessage { text, duration } => {
                sink(SessionEvent::Osd { text, duration });
            }
            Inbound::ForceKeepAlive(interval) => keepalive = interval / 2,
            Inbound::Group(update) => on_group_update(update, sink, group, clock_wake),
            Inbound::Ignored => {}
        }
    }
}
