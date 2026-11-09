use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicUsize;

use jellyfin::{format_utc, local_now_ms, new_device_id, parse_utc_ms};
use serde_json::{Value, json};
use tungstenite::Message;

use super::group::{CLOCK_SAMPLES, command_delay_ms, send_group_wait_report};
use super::supervisor::quick_connect_text;
use super::*;
use crate::server::APP;
use crate::server::state::State;

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
    flaky_group_server(seen, frames, offset_ms, failing, usize::MAX, false)
}

/// As `group_server`, with every request to `path` dropped unanswered.
fn group_server_hanging_up(
    seen: Sender<(String, String)>,
    frames: Receiver<Option<String>>,
    path: &'static str,
) -> String {
    flaky_group_server(seen, frames, 0, Some(path), usize::MAX, true)
}

/// As `group_server`, with only the first `failures` matching requests
/// failing; with `hang_up`, by dropping the connection instead of a 500.
fn flaky_group_server(
    seen: Sender<(String, String)>,
    frames: Receiver<Option<String>>,
    offset_ms: i64,
    failing: Option<&'static str>,
    failures: usize,
    hang_up: bool,
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
                if !(fails && hang_up) {
                    reply_json(&stream, status, &reply);
                }
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
    while let Ok(event) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
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
    let connected = |e: &SessionEvent| matches!(e, SessionEvent::Log(l) if l.contains("connected"));
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
    let url = flaky_group_server(seen_tx, frames_rx, 0, Some("/SyncPlay/Ready"), 2, false);
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
fn a_ready_lost_to_the_network_is_retried_and_keeps_the_group() {
    let (seen_tx, seen) = mpsc::channel();
    let (frames, frames_rx) = mpsc::channel();
    let url = flaky_group_server(seen_tx, frames_rx, 0, Some("/SyncPlay/Ready"), 3, true);
    let (session, events, dir) = connected_session(&url);
    let soon = Duration::from_secs(5);
    frames.send(Some(group_frame("GroupJoined", "g1"))).unwrap();
    assert!(wait_for(&events, soon, |e| has_joined(e, "g1")));

    session.submit(Job::Group(ready_request()));
    let mut requests = Vec::new();
    for _ in 0..4 {
        requests.extend(requests_until(&seen, "/SyncPlay/Ready"));
    }
    assert_eq!(lines_with(&requests, "/SyncPlay/Leave"), 0, "left too soon");
    let stamps: Vec<i64> = requests
        .iter()
        .filter(|(line, _)| line.contains("/SyncPlay/Ready"))
        .map(|(_, body)| {
            let body: Value = serde_json::from_str(body).unwrap();
            parse_utc_ms(body["When"].as_str().unwrap()).unwrap()
        })
        .collect();
    assert!(stamps.is_sorted_by(|a, b| a < b), "stale When: {stamps:?}");

    let more: Vec<_> =
        std::iter::from_fn(|| seen.recv_timeout(Duration::from_secs(1)).ok()).collect();
    assert_eq!(
        lines_with(&more, "/SyncPlay/Ready"),
        0,
        "the last one worked"
    );
    assert_eq!(lines_with(&more, "/SyncPlay/Leave"), 0);
    assert!(!events.try_iter().any(|e| is_lost_contact(&e)));

    session.shutdown(Duration::ZERO);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_lost_ready_stops_when_the_socket_goes_and_keeps_the_group() {
    let (seen_tx, seen) = mpsc::channel();
    let (frames, frames_rx) = mpsc::channel();
    let url = group_server_hanging_up(seen_tx, frames_rx, "/SyncPlay/Ready");
    let (session, events, dir) = connected_session(&url);
    let soon = Duration::from_secs(5);
    frames.send(Some(group_frame("GroupJoined", "g1"))).unwrap();
    assert!(wait_for(&events, soon, |e| has_joined(e, "g1")));

    session.submit(Job::Group(ready_request()));
    requests_until(&seen, "/SyncPlay/Ready");
    frames.send(None).unwrap();
    assert!(wait_for(&events, soon, is_connection_lost));
    // A post already on its way when the socket went may still land.
    thread::sleep(Duration::from_millis(400));
    let before_the_drop_settled: Vec<_> = seen.try_iter().collect();
    assert_eq!(lines_with(&before_the_drop_settled, "/SyncPlay/Leave"), 0);

    let after: Vec<_> =
        std::iter::from_fn(|| seen.recv_timeout(Duration::from_secs(3)).ok()).collect();
    assert_eq!(
        lines_with(&after, "/SyncPlay/Ready"),
        0,
        "a Ready before the Join draws NotInGroup: {after:?}"
    );
    assert_eq!(lines_with(&after, "/SyncPlay/Leave"), 0);
    assert!(!events.try_iter().any(|e| is_lost_contact(&e)));

    session.shutdown(Duration::ZERO);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_ready_lost_to_the_network_past_the_limit_gives_up() {
    let (seen_tx, seen) = mpsc::channel();
    let (_frames, frames_rx) = mpsc::channel();
    let url = group_server_hanging_up(seen_tx, frames_rx, "/SyncPlay/Ready");
    let base = BaseUrl::parse(&url).unwrap();
    let auth = Secret::new("auth".to_owned());
    let creds = Creds {
        client: Client::new(base.clone(), APP, auth.clone()),
        user_id: "user".to_owned(),
        auth,
        media_base: base,
    };
    let (tx, events) = mpsc::channel();
    let sink: Sink = Arc::new(move |event| {
        let _ = tx.send(event);
    });
    let group = Mutex::new(GroupState {
        joined: Some("g".to_owned()),
        in_group: true,
        offset_ms: 0,
    });

    send_group_wait_report(
        &ready_request(),
        Some(&creds),
        &sink,
        &AtomicBool::new(false),
        &group,
        Duration::from_millis(100),
    );

    let requests = requests_until(&seen, "/SyncPlay/Leave");
    assert_eq!(
        lines_with(&requests, "/SyncPlay/Ready"),
        2,
        "one retry, then past the limit"
    );
    assert!(events.try_iter().any(|e| is_lost_contact(&e)));
    assert_eq!(locked(&group).joined, None);
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
