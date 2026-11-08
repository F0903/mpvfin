//! End-to-end casting tests against the Docker Jellyfin and a real mpv.
//! Run: `cargo test -p mpvfin-e2e -- --ignored --test-threads=1`

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mpvfin_e2e::{
    Player, PlayerOptions, Server, StallingServer, State, TempDir, poll_until, reported_position,
    write_config,
};
use serde_json::{Value, json};

const T10: Duration = Duration::from_secs(10);
const T20: Duration = Duration::from_secs(20);
const STATE_FILE: &str = "script-opts/mpvfin-state.json";

const fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn fresh_id() -> String {
    mpvfin_jellyfin::new_device_id()
}

fn no_token(device_id: &str) -> State {
    State {
        device_id: device_id.to_owned(),
        server: None,
        user_id: None,
        access_token: None,
    }
}

/// A logged-in plugin connected to Jellyfin.
struct Cast {
    server: Server,
    player: Player,
    device: String,
    session: String,
    /// Episode ids, E01 first.
    items: Vec<String>,
}

fn options(server: &Server, state: State, mpv_conf: &str) -> PlayerOptions {
    PlayerOptions {
        server: server.url().to_owned(),
        state,
        mpv_conf: mpv_conf.to_owned(),
        config_dir: None,
    }
}

/// Waits until `device` can receive commands; returns its session id.
#[track_caller]
fn connected_session(server: &Server, player: &Player, device: &str) -> String {
    // The log line comes after the WebSocket opened; a command sent earlier is lost.
    player.wait_log("connected to Jellyfin as ", T20);
    let session = poll_until(T20, || {
        server
            .mpvfin_session(device)
            .filter(|s| s["SupportsMediaControl"] == true)
    });
    session["Id"].as_str().expect("session id").to_owned()
}

fn cast_with(mpv_conf: &str, config_dir: Option<PathBuf>) -> Cast {
    let server = Server::connect();
    let device = fresh_id();
    let mut opts = options(&server, server.device_login(&device), mpv_conf);
    opts.config_dir = config_dir;
    let player = Player::launch(opts);
    let session = connected_session(&server, &player, &device);
    let items = server.episodes().into_iter().map(|(id, _)| id).collect();
    Cast {
        server,
        player,
        device,
        session,
        items,
    }
}

fn cast() -> Cast {
    cast_with("", None)
}

impl Cast {
    #[track_caller]
    fn session_where(&self, f: impl Fn(&Value) -> bool) -> Value {
        poll_until(T20, || self.server.mpvfin_session(&self.device).filter(&f))
    }

    #[track_caller]
    fn wait_playing(&self, item: &str) -> Value {
        self.session_where(|s| s["NowPlayingItem"]["Id"] == item)
    }

    /// Plays `item` from `start` and waits until mpv is playing it.
    #[track_caller]
    fn play_one(&self, item: &str, start: Duration) {
        self.server.play(&self.session, &[item], 0, start);
        self.wait_playing(item);
    }

    #[track_caller]
    fn wait_reported_position(&self, low: Duration, high: Duration) -> Value {
        self.session_where(|s| reported_position(s).is_some_and(|p| p >= low && p <= high))
    }

    #[track_caller]
    fn wait_mpv(&self, property: &str, expected: &Value) {
        wait_mpv(&self.player, property, expected);
    }

    /// `(Index of second audio, Index of external subtitle)`.
    fn e01_streams(&self) -> (i64, i64) {
        let streams = self.server.media_streams(&self.items[0]);
        let index = |s: &Value| s["Index"].as_i64().expect("Index");
        let mut audio: Vec<i64> = streams
            .iter()
            .filter(|s| s["Type"] == "Audio")
            .map(index)
            .collect();
        audio.sort_unstable();
        let external = streams
            .iter()
            .find(|s| s["Type"] == "Subtitle" && s["IsExternal"] == true)
            .map(index)
            .expect("external subtitle");
        (audio[1], external)
    }
}

#[track_caller]
fn wait_mpv(player: &Player, property: &str, expected: &Value) {
    let mut last = Value::Null;
    let reached = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        poll_until(T20, || {
            last = player.get(property);
            (last == *expected).then_some(())
        });
    }));
    assert!(
        reached.is_ok(),
        "mpv {property}: expected {expected}, last {last}"
    );
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, files);
            } else if !path.ends_with(STATE_FILE) {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn quick_connect_login_registers_session() {
    let server = Server::connect();
    let device = fresh_id();
    let dir = TempDir::new();
    let mut opts = options(&server, no_token(&device), "");
    opts.config_dir = Some(dir.path().to_owned());
    let player = Player::launch(opts);

    let line = player.wait_log("Quick Connect code: ", T20);
    let code = line
        .rsplit("Quick Connect code: ")
        .next()
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(code.len(), 6, "{line}");
    assert!(code.bytes().all(|b| b.is_ascii_digit()), "{line}");
    server.authorize_quick_connect(&code);

    poll_until(T20, || {
        server
            .mpvfin_session(&device)
            .filter(|s| s["SupportsMediaControl"] == true)
    });
    player.quit();

    let text = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
    let state: State = serde_json::from_str(&text).unwrap();
    assert_eq!(state.device_id, device);
    assert!(state.access_token.is_some_and(|t| !t.is_empty()));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn remote_play_pause_seek_stop_report_back() {
    let c = cast();
    let e01 = c.items[0].as_str();
    c.server.play(&c.session, &[e01], 0, secs(30));
    c.session_where(|s| {
        s["NowPlayingItem"]["Id"] == e01 && reported_position(s).is_some_and(|p| p >= secs(30))
    });

    c.server.playstate(&c.session, "Pause", None);
    c.session_where(|s| s["PlayState"]["IsPaused"] == true);
    c.wait_mpv("pause", &json!(true));

    c.server.playstate(&c.session, "Seek", Some(secs(60)));
    c.wait_reported_position(secs(58), secs(65));

    c.server
        .command(&c.session, "SetVolume", json!({"Volume": "40"}));
    c.wait_mpv("volume", &json!(40.0));
    c.session_where(|s| s["PlayState"]["VolumeLevel"] == 40);

    c.wait_mpv("pause", &json!(true));
    c.server.playstate(&c.session, "Stop", None);
    c.session_where(|s| s.get("NowPlayingItem").is_none());
    // A remote pause belongs to the item, so it must not outlive it.
    c.wait_mpv("idle-active", &json!(true));
    c.wait_mpv("pause", &json!(false));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn queue_start_index_and_next_previous() {
    let c = cast();
    let ids: Vec<&str> = c.items.iter().map(String::as_str).collect();
    c.server.play(&c.session, &ids, 1, Duration::ZERO);
    c.wait_mpv("playlist-count", &json!(3));
    c.wait_playing(ids[1]);

    c.server.playstate(&c.session, "NextTrack", None);
    c.wait_playing(ids[2]);
    c.server.playstate(&c.session, "PreviousTrack", None);
    c.wait_playing(ids[1]);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn local_pause_and_seek_are_reported() {
    let c = cast();
    c.play_one(&c.items[0], Duration::ZERO);

    c.player.command(&["set", "pause", "yes"]);
    c.session_where(|s| s["PlayState"]["IsPaused"] == true);

    c.player.command(&["seek", "90", "absolute"]);
    c.wait_reported_position(secs(88), secs(95));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn remote_track_changes_select_mpv_tracks() {
    let c = cast();
    c.play_one(&c.items[0], Duration::ZERO);
    let (second_audio, external_sub) = c.e01_streams();

    c.server.command(
        &c.session,
        "SetAudioStreamIndex",
        json!({"Index": second_audio.to_string()}),
    );
    c.wait_mpv("aid", &json!(2));

    c.server.command(
        &c.session,
        "SetSubtitleStreamIndex",
        json!({"Index": external_sub.to_string()}),
    );
    c.wait_mpv("sid", &json!(3));
    let tracks = c.player.get("track-list");
    let selected = tracks
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"] == "sub" && t["id"] == 3)
        .expect("subtitle track 3");
    assert_eq!(selected["external"], true, "{selected}");

    c.server
        .command(&c.session, "SetSubtitleStreamIndex", json!({"Index": "-1"}));
    c.wait_mpv("sid", &json!(false));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn config_dir_untouched_and_token_absent_from_log() {
    let mpv_conf = "save-position-on-quit=yes\nwatch-later-dir=~~/watch_later\n";
    let server = Server::connect();
    let device = fresh_id();
    let state = server.device_login(&device);
    let token = state.access_token.clone().unwrap();
    let dir = TempDir::new();
    let mut opts = options(&server, state, mpv_conf);
    opts.config_dir = Some(dir.path().to_owned());
    write_config(dir.path(), &opts);
    let before = snapshot(dir.path());

    let player = Player::launch(opts);
    let session = connected_session(&server, &player, &device);
    let e01 = server.episodes()[0].0.clone();
    server.play(&session, &[&e01], 0, Duration::ZERO);
    poll_until(T20, || {
        server
            .mpvfin_session(&device)
            .filter(|s| s["NowPlayingItem"]["Id"] == e01.as_str())
    });
    std::thread::sleep(secs(5));
    let tracks = player.get("track-list");
    assert!(
        tracks
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "sub" && t["external"] == true),
        "external subtitle not loaded: {tracks}"
    );
    let (_, log) = player.quit_with_log();

    assert_eq!(before, snapshot(dir.path()), "config dir changed");
    let watch_later = dir.path().join("watch_later");
    assert!(
        !watch_later.exists() || fs::read_dir(&watch_later).unwrap().next().is_none(),
        "watch_later was written"
    );

    assert!(log.contains("Stream.subrip"), "log is too short to judge");
    let lower = log.to_ascii_lowercase();
    assert!(
        !lower.contains(&token.to_ascii_lowercase()),
        "token in the mpv log"
    );
    for forbidden in ["authorization", "apikey=", "api_key="] {
        assert!(!lower.contains(forbidden), "{forbidden} in the mpv log");
    }
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn second_window_stays_dormant_until_first_closes() {
    let dir = TempDir::new();
    let first = cast_with("", Some(dir.path().to_owned()));
    let (server, device) = (&first.server, &first.device);
    let mut opts = options(server, no_token(device), "");
    opts.config_dir = Some(first.player.config_dir().to_owned());
    let second = Player::launch(opts);

    second.wait_log("another mpv window is the Jellyfin cast target", T20);
    std::thread::sleep(secs(3));
    assert!(
        !second.log_text().contains("connected to Jellyfin"),
        "second window connected while the first was alive"
    );

    first.player.quit();
    second.wait_log("connected to Jellyfin", T20);
    poll_until(T20, || server.mpvfin_session(device));
    second.quit();
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn mpv_stays_responsive_and_quits_when_server_unreachable() {
    let unreachable = "http://192.0.2.1:8096";
    let server = Server::connect();
    let device = fresh_id();
    let mut state = server.device_login(&device);
    // The plugin drops a token saved for another server.
    state.server = Some(unreachable.to_owned());
    let player = Player::launch(PlayerOptions {
        server: unreachable.to_owned(),
        state,
        mpv_conf: String::new(),
        config_dir: None,
    });

    std::thread::sleep(secs(2));
    let started = Instant::now();
    assert_eq!(player.get("pause"), json!(false));
    let answered = started.elapsed();
    assert!(answered < secs(1), "IPC answered in {answered:?}");

    let quit = player.quit();
    assert!(quit < secs(5), "quit took {quit:?}");
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn quit_is_bounded_while_resolve_stalls() {
    let stub = StallingServer::start("stalled-item");
    let player = Player::launch(PlayerOptions {
        server: stub.url().to_owned(),
        state: State {
            device_id: fresh_id(),
            server: Some(stub.url().to_owned()),
            user_id: Some("user".to_owned()),
            access_token: Some("token".to_owned()),
        },
        mpv_conf: String::new(),
        config_dir: None,
    });
    player.wait_log("connected to Jellyfin", T20);
    poll_until(T20, || stub.item_requested().then_some(()));

    let quit = player.quit();
    assert!(quit < secs(5), "quit took {quit:?}");
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn quitting_mid_playback_saves_resume_position() {
    let c = cast();
    let e02 = c.items[1].clone();
    c.server.clear_resume(&e02);
    assert_eq!(c.server.resume_position(&e02), Duration::ZERO);

    c.play_one(&e02, Duration::ZERO);
    c.server.playstate(&c.session, "Seek", Some(secs(45)));
    c.wait_reported_position(secs(45), secs(48));

    // The seek's report already saved ~45 s, and the next tick is 10 s away:
    // only the stop report can carry the position reached by now.
    std::thread::sleep(secs(3));
    c.player.quit();
    let resume = poll_until(T10, || {
        Some(c.server.resume_position(&e02)).filter(|p| *p >= secs(47))
    });
    assert!(resume <= secs(50), "resume position {resume:?}");
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn local_file_during_cast_reports_stop_and_stays_untouched() {
    let c = cast();
    c.play_one(&c.items[0], Duration::ZERO);

    let local = "av://lavfi:testsrc=size=64x64:rate=1";
    c.player.command(&["loadfile", local, "replace"]);
    poll_until(T10, || {
        c.server
            .mpvfin_session(&c.device)
            .filter(|s| s.get("NowPlayingItem").is_none())
    });
    c.wait_mpv("path", &json!(local));
    c.wait_mpv("force-media-title", &json!(""));
    c.wait_mpv("http-header-fields", &json!([]));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn rejected_token_falls_back_to_quick_connect() {
    let server = Server::connect();
    let device = fresh_id();
    let mut state = server.device_login(&device);
    state.access_token = Some("invalid".to_owned());
    let player = Player::launch(options(&server, state, ""));
    player.wait_log("Quick Connect code: ", T20);
    player.quit();
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn seek_beyond_demuxer_cache_works_through_proxy() {
    let c = cast_with(
        "cache=no
demuxer-max-bytes=512KiB
demuxer-max-back-bytes=0
",
        None,
    );
    let e01 = c.items[0].as_str();
    c.play_one(e01, Duration::ZERO);

    c.server.playstate(&c.session, "Seek", Some(secs(100)));
    let landed = poll_until(T20, || {
        let in_mpv = c.player.get("time-pos").as_f64()?;
        let reported = reported_position(&c.server.mpvfin_session(&c.device)?)?;
        ((98.0..=108.0).contains(&in_mpv) && (secs(98)..=secs(108)).contains(&reported))
            .then_some(in_mpv)
    });

    // A false EOF would end the item or restart the clock in the next one.
    poll_until(T10, || {
        c.player
            .get("time-pos")
            .as_f64()
            .filter(|p| *p >= landed + 1.0)
    });
    assert_eq!(c.player.get("eof-reached"), json!(false));
    let session = c.server.mpvfin_session(&c.device).expect("session");
    assert_eq!(session["NowPlayingItem"]["Id"], e01, "item stopped playing");

    let (_, log) = c.player.quit_with_log();
    assert!(
        !log.contains("Too many packets"),
        "demuxer overflowed, so this did not test a normal seek"
    );
}
