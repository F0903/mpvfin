//! Logging in, the state file, and leaving the viewer's mpv config alone.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use e2e::{Player, PlayerOptions, Server, State, TempDir, poll_until, write_config};
use serde_json::json;

use crate::{
    STATE_FILE, T10, T20, cast_with, connected_session, fresh_id, no_token, options, secs,
};

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
fn rejected_token_falls_back_to_quick_connect() {
    let server = Server::connect();
    let device = fresh_id();
    let mut state = server.device_login(&device);
    state.access_token = Some("invalid".to_owned());
    let player = Player::launch(options(&server, state, ""));
    player.wait_log("Quick Connect code: ", T20);
    player.quit();
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
