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
const KEY_SECTION: &str = "mpvfin-skip-key";
const CLICK_SECTION: &str = "mpvfin-skip-click";
const NEXT_KEY_SECTION: &str = "mpvfin-next-up-key";
const NEXT_CLICK_SECTION: &str = "mpvfin-next-up-click";

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
        default_bindings: false,
        input_conf: String::new(),
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

fn cast_adjusted(adjust: impl FnOnce(&mut PlayerOptions)) -> Cast {
    let server = Server::connect();
    let device = fresh_id();
    let mut opts = options(&server, server.device_login(&device), "");
    adjust(&mut opts);
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

fn cast_with(mpv_conf: &str, config_dir: Option<PathBuf>) -> Cast {
    cast_adjusted(|opts| {
        mpv_conf.clone_into(&mut opts.mpv_conf);
        opts.config_dir = config_dir;
    })
}

fn cast() -> Cast {
    cast_with("", None)
}

/// A cast with mpv's own keys live and no played state or resume left over.
fn skip_cast(input_conf: &str) -> Cast {
    let c = cast_adjusted(|opts| {
        opts.default_bindings = true;
        input_conf.clone_into(&mut opts.input_conf);
    });
    for item in &c.items {
        c.server.clear_resume(item);
    }
    c
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

    fn position(&self) -> f64 {
        self.player.get("time-pos").as_f64().expect("time-pos")
    }

    /// Waits for the button; its key section is enabled only while it is up.
    #[track_caller]
    fn wait_button(&self) {
        poll_until(T20, || {
            section_active(&self.player, KEY_SECTION).then_some(())
        });
    }

    /// Waits for the Next Up card; its key section is enabled only while it is up.
    #[track_caller]
    fn wait_next_up(&self) {
        poll_until(T20, || {
            section_active(&self.player, NEXT_KEY_SECTION).then_some(())
        });
    }

    /// Waits until the card's input is gone; a press keeps it on for a moment.
    #[track_caller]
    fn wait_next_up_off(&self) {
        poll_until(secs(2), || {
            (!section_active(&self.player, NEXT_KEY_SECTION)
                && !section_active(&self.player, NEXT_CLICK_SECTION))
            .then_some(())
        });
    }

    /// Waits up to 5 s for mpv's position to land in `low..=high`.
    #[track_caller]
    fn wait_position(&self, low: f64, high: f64) {
        poll_until(secs(5), || {
            self.player
                .get("time-pos")
                .as_f64()
                .filter(|p| (low..=high).contains(p))
        });
    }

    /// Waits up to 5 s for `item`, less than the playback `assert_far_from` left.
    #[track_caller]
    fn wait_next(&self, item: &str) {
        poll_until(secs(5), || {
            self.server
                .mpvfin_session(&self.device)
                .filter(|s| s["NowPlayingItem"]["Id"] == item)
        });
    }

    /// Asserts playback alone cannot reach `target` s within `wait_position` or `wait_next`.
    #[track_caller]
    fn assert_far_from(&self, target: f64) {
        let position = self.position();
        assert!(position + 6.0 < target, "too close to {target}: {position}");
    }

    /// Waits until both sections are off; a skip keeps them on for a moment.
    #[track_caller]
    fn wait_sections_off(&self) {
        poll_until(secs(2), || {
            (!section_active(&self.player, KEY_SECTION)
                && !section_active(&self.player, CLICK_SECTION))
            .then_some(())
        });
    }

    /// Moves the pointer onto the button and waits until clicks are the button's.
    #[track_caller]
    fn hover_button(&self) {
        // Under --vo=null only this makes mpv treat the pointer as hovering.
        self.press("MOUSE_ENTER");
        self.player.command(&["mouse", "1150", "600"]); // inside the 1280x720 button
        self.wait_click_section(true);
    }

    #[track_caller]
    fn wait_click_section(&self, active: bool) {
        poll_until(T10, || {
            (section_active(&self.player, CLICK_SECTION) == active).then_some(())
        });
    }

    /// A left click. `keypress` has no down state, so mpv can't see a double
    /// click in it.
    fn click(&self) {
        self.player.command(&["keydown", "MBTN_LEFT"]);
        self.player.command(&["keyup", "MBTN_LEFT"]);
    }

    fn press(&self, key: &str) {
        self.player.command(&["keypress", key]);
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

/// Whether `section` is enabled; mpv gives a disabled section priority -1.
fn section_active(player: &Player, section: &str) -> bool {
    let bindings = player.get("input-bindings");
    bindings
        .as_array()
        .expect("input-bindings")
        .iter()
        .any(|b| {
            b["section"] == section && b["priority"].as_i64().is_some_and(|priority| priority >= 0)
        })
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
fn local_pause_then_remote_unpause_does_not_pause_the_next_item() {
    let c = cast();
    let ids: Vec<&str> = c.items.iter().map(String::as_str).collect();
    c.server.play(&c.session, &ids, 0, Duration::ZERO);
    c.wait_playing(ids[0]);

    c.player.command(&["set", "pause", "yes"]);
    c.session_where(|s| s["PlayState"]["IsPaused"] == true);
    c.server.playstate(&c.session, "Unpause", None);
    c.session_where(|s| s["PlayState"]["IsPaused"] == false);
    c.wait_mpv("pause", &json!(false));

    c.server.playstate(&c.session, "NextTrack", None);
    c.wait_playing(ids[1]);
    assert_eq!(c.player.get("pause"), json!(false));
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
fn cast_selecting_an_external_subtitle_applies_after_its_sub_add() {
    let c = cast();
    let (_, external_sub) = c.e01_streams();
    c.server
        .play_with_subtitle(&c.session, &c.items[0], external_sub);
    poll_until(T20, || {
        let tracks = c.player.get("track-list");
        tracks.as_array()?.iter().find(|t| {
            t["type"] == "sub" && t["id"] == 3 && t["external"] == true && t["selected"] == true
        })?;
        Some(())
    });
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
    // A per-file override applies after load, so the user's setting shows here.
    assert_eq!(player.get("save-position-on-quit"), json!(true));
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
    let (quit, log) = player.quit_watch_later();

    assert!(quit < secs(5), "quit-watch-later took {quit:?}");
    assert!(
        !dir.path().join("watch_later").exists(),
        "watch_later was written"
    );
    assert_eq!(before, snapshot(dir.path()), "config dir changed");

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
    // Longer than the 5 s lock retry, so a retry that wrongly succeeds is seen.
    std::thread::sleep(secs(6));
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
        default_bindings: false,
        input_conf: String::new(),
    });

    std::thread::sleep(secs(2));
    player.wait_log("[mpvfin] loaded", T10);
    assert!(
        !player.log_text().contains("no server configured"),
        "the plugin never got as far as the unreachable server"
    );
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
        default_bindings: false,
        input_conf: String::new(),
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

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn button_appears_in_intro_and_enter_skips_it() {
    let c = skip_cast("");
    let e01 = c.items[0].as_str();
    c.play_one(e01, secs(8));
    c.wait_button();
    c.assert_far_from(20.0);

    c.press("ENTER");
    c.wait_position(20.0, 24.0);
    c.wait_playing(e01);
    c.wait_sections_off();
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn enter_returns_to_mpv_after_the_segment() {
    let c = skip_cast("");
    let (e01, e02) = (c.items[0].as_str(), c.items[1].as_str());
    c.server.play(&c.session, &[e01, e02], 0, secs(30));
    c.wait_playing(e01);
    assert!(
        !section_active(&c.player, KEY_SECTION),
        "no segment at 30 s"
    );

    c.press("ENTER");
    c.wait_playing(e02);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn click_on_button_skips_and_click_elsewhere_does_not() {
    let c = skip_cast("");
    c.play_one(&c.items[0], secs(8));
    c.wait_button();

    // Under --vo=null only this makes mpv treat the pointer as hovering.
    c.press("MOUSE_ENTER");
    c.player.command(&["mouse", "10", "10"]);
    c.press("MBTN_LEFT");
    // No state to poll: this waits for a skip that must not happen.
    std::thread::sleep(Duration::from_millis(500));
    assert!(c.position() < 20.0, "click off the button skipped");

    c.player.command(&["mouse", "1150", "600"]); // inside the 1280x720 button
    poll_until(T10, || {
        section_active(&c.player, CLICK_SECTION).then_some(())
    });
    c.assert_far_from(20.0);
    c.press("MBTN_LEFT");
    c.wait_position(20.0, 24.0);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn double_enter_skips_only_once() {
    let c = skip_cast("");
    let e01 = c.items[0].as_str();
    c.play_one(e01, secs(8));
    c.wait_button();
    c.assert_far_from(20.0);

    c.press("ENTER");
    std::thread::sleep(Duration::from_millis(80));
    c.press("ENTER");
    c.wait_position(20.0, 24.0);
    c.wait_sections_off();
    c.wait_playing(e01);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn double_click_skips_only_once_and_leaves_fullscreen_alone() {
    let c = skip_cast("");
    let e01 = c.items[0].as_str();
    c.play_one(e01, secs(8));
    c.wait_button();
    c.hover_button();
    c.assert_far_from(20.0);
    let fullscreen = c.player.get("fullscreen");

    c.click();
    std::thread::sleep(Duration::from_millis(50));
    c.click();
    c.wait_position(20.0, 24.0);
    c.wait_sections_off();
    c.wait_playing(e01);
    assert_eq!(c.player.get("fullscreen"), fullscreen);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn an_open_uosc_menu_keeps_clicks_off_the_button() {
    let c = skip_cast("");
    c.play_one(&c.items[0], secs(8));
    c.wait_button();
    c.hover_button();

    c.player
        .command(&["set", "user-data/uosc/menu/type", "test"]);
    c.wait_click_section(false);
    c.press("MBTN_LEFT");
    // No state to poll: this waits for a skip that must not happen.
    std::thread::sleep(Duration::from_millis(500));
    assert!(c.position() < 20.0, "the click went through the menu");
    assert!(!section_active(&c.player, CLICK_SECTION));

    // uosc closes a menu by setting the property to nil, not by deleting it.
    c.player.set_null("user-data/uosc/menu/type");
    c.wait_click_section(true);
    c.assert_far_from(20.0);
    c.press("MBTN_LEFT");
    c.wait_position(20.0, 24.0);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn user_input_conf_binding_wins_over_the_default_hotkey() {
    let c = skip_cast("ENTER set pause yes\n");
    c.play_one(&c.items[0], secs(8));
    c.wait_button();

    c.press("ENTER");
    c.wait_mpv("pause", &json!(true));
    assert!(c.position() < 20.0, "ENTER skipped");

    c.player.command(&["script-binding", "mpvfin/skip"]);
    c.wait_position(20.0, 24.0);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn skipping_credits_at_end_of_file_marks_played_and_plays_next() {
    let c = skip_cast("");
    let (e01, e02) = (c.items[0].as_str(), c.items[1].as_str());
    // From 30 s, so the only button that can come up is the credits'.
    c.server.play(&c.session, &[e01, e02], 0, secs(30));
    c.wait_playing(e01);
    c.player.command(&["seek", "102", "absolute"]);
    c.wait_button();
    c.assert_far_from(120.0);

    c.press("ENTER");
    c.wait_next(e02);
    poll_until(T10, || c.server.is_played(e01).then_some(()));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn skipping_credits_at_end_of_file_while_paused_advances() {
    let c = skip_cast("");
    let (e01, e02) = (c.items[0].as_str(), c.items[1].as_str());
    c.server.play(&c.session, &[e01, e02], 0, secs(30));
    c.wait_playing(e01);
    c.player.command(&["seek", "102", "absolute"]);
    c.wait_button();

    c.player.command(&["set", "pause", "yes"]);
    // Reported, so the plugin has seen the pause before the skip.
    c.session_where(|s| s["PlayState"]["IsPaused"] == true);
    c.press("ENTER");
    poll_until(secs(8), || {
        c.server
            .mpvfin_session(&c.device)
            .filter(|s| s["NowPlayingItem"]["Id"] == e02)
    });
    assert_eq!(c.player.get("pause"), json!(false));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn skipping_credits_mid_file_marks_played_before_the_watched_point() {
    let c = skip_cast("");
    let e02 = c.items[1].as_str();
    // From 30 s, past E02's recap, so the first button can only be the credits'.
    c.play_one(e02, secs(30));
    c.wait_position(30.0, 34.0);
    // Every enable-section is logged, so even a brief button before 30 s shows.
    let log = c.player.log_text();
    assert!(
        !log.lines()
            .any(|l| l.contains("Run command: enable-section") && l.contains(KEY_SECTION)),
        "button shown before the requested start"
    );
    c.player.command(&["seek", "52", "absolute"]);
    c.wait_button();
    c.assert_far_from(70.0);

    c.press("ENTER");
    c.wait_position(70.0, 74.0);
    c.wait_playing(e02);
    poll_until(T10, || c.server.is_played(e02).then_some(()));
    assert!(c.position() < 108.0, "reached the 90% watched point");
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn skipping_the_preview_at_end_of_file_ends_the_item() {
    let c = skip_cast("");
    let (e02, e03) = (c.items[1].as_str(), c.items[2].as_str());
    // Jellyfin queues the rest of the season behind one episode, so E03 follows.
    c.play_one(e02, secs(30));
    c.player.command(&["seek", "112", "absolute"]);
    c.wait_button();
    c.assert_far_from(120.0);

    c.press("ENTER");
    c.wait_next(e03);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn next_up_comes_with_the_credits_and_n_plays_the_next_item() {
    let c = skip_cast("");
    let (e01, e02) = (c.items[0].as_str(), c.items[1].as_str());
    c.server.play(&c.session, &[e01, e02], 0, secs(30));
    c.wait_playing(e01);
    assert!(
        !section_active(&c.player, NEXT_KEY_SECTION),
        "no card before the credits"
    );
    c.player.command(&["seek", "102", "absolute"]);
    c.wait_next_up();
    c.assert_far_from(120.0);
    assert!(
        section_active(&c.player, KEY_SECTION),
        "Skip Credits stays up below the card"
    );

    c.press("n");
    c.wait_next(e02);
    poll_until(T10, || c.server.is_played(e01).then_some(()));
    c.wait_next_up_off();
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn next_up_names_the_next_episode() {
    let c = skip_cast("");
    // mpv logs `osd-overlay` only at trace level.
    c.player
        .command(&["set", "msg-level", "all=debug,cplayer=trace"]);
    let (e01, e02) = (c.items[0].as_str(), c.items[1].as_str());
    c.server.play(&c.session, &[e01, e02], 0, secs(102));
    c.wait_playing(e01);
    c.wait_next_up();
    poll_until(T10, || {
        c.player.log_text().contains("S01E02 · ").then_some(())
    });
    poll_until(T10, || {
        c.player.log_text().contains("overlay-add").then_some(())
    });

    c.press("n");
    c.wait_next(e02);
    poll_until(T10, || {
        c.player.log_text().contains("overlay-remove").then_some(())
    });
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn next_up_without_credits_is_the_last_thirty_seconds_and_clicks_through() {
    let c = skip_cast("");
    let (e03, e01) = (c.items[2].as_str(), c.items[0].as_str());
    c.server.play(&c.session, &[e03, e01], 0, secs(10));
    c.wait_playing(e03);
    c.player.command(&["seek", "80", "absolute"]);
    c.wait_position(80.0, 84.0);
    assert!(
        !section_active(&c.player, NEXT_KEY_SECTION),
        "more than 30 s left"
    );
    c.player.command(&["seek", "95", "absolute"]);
    c.wait_next_up();

    c.press("MOUSE_ENTER");
    c.player.command(&["mouse", "1000", "530"]); // inside the 1280x720 Play Now, with a picture beside the text
    poll_until(T10, || {
        section_active(&c.player, NEXT_CLICK_SECTION).then_some(())
    });
    let fullscreen = c.player.get("fullscreen");
    c.click();
    std::thread::sleep(Duration::from_millis(50));
    c.click();
    c.wait_next(e01);
    assert_eq!(c.player.get("fullscreen"), fullscreen);
    c.wait_next_up_off();
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn next_up_stays_away_from_the_last_entry_until_one_is_queued() {
    let c = skip_cast("");
    let (e03, e01) = (c.items[2].as_str(), c.items[0].as_str());
    c.play_one(e03, secs(10));
    c.player.command(&["seek", "95", "absolute"]);
    c.wait_position(95.0, 99.0);
    c.player.command(&["set", "pause", "yes"]);
    assert!(
        !section_active(&c.player, NEXT_KEY_SECTION),
        "nothing follows E03"
    );

    c.player
        .command(&["loadfile", &format!("mpvfin://{e01}"), "append"]);
    // Noticed on the 10 s progress tick.
    poll_until(T20, || {
        section_active(&c.player, NEXT_KEY_SECTION).then_some(())
    });
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn user_input_conf_binding_wins_over_the_next_up_hotkey() {
    let c = skip_cast("n set pause yes\n");
    let (e03, e01) = (c.items[2].as_str(), c.items[0].as_str());
    c.server.play(&c.session, &[e03, e01], 0, secs(95));
    c.wait_playing(e03);
    c.wait_next_up();
    c.assert_far_from(120.0);

    c.press("n");
    c.wait_mpv("pause", &json!(true));
    c.wait_playing(e03);

    c.player.command(&["script-binding", "mpvfin/next-up"]);
    c.wait_next(e01);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn button_appears_for_a_cast_from_the_start_of_the_file() {
    let c = skip_cast("");
    // E02 opens with a recap, so a button needs the settled position at 0 s.
    c.play_one(&c.items[1], Duration::ZERO);
    c.wait_button();
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn no_segments_means_enter_stays_mpvs() {
    let c = skip_cast("");
    let (e03, e01) = (c.items[2].as_str(), c.items[0].as_str());
    c.server.play(&c.session, &[e03, e01], 0, secs(10));
    c.wait_playing(e03);
    assert!(
        !section_active(&c.player, KEY_SECTION),
        "E03 has no segments"
    );

    c.press("ENTER");
    c.wait_playing(e01);
}
