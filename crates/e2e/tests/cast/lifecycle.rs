//! Quitting, resuming, stalls, and local files in the middle of a cast.

use std::time::Duration;

use e2e::{Player, PlayerOptions, StallingServer, State, poll_until, reported_position};
use serde_json::json;

use crate::{T10, T20, cast, cast_with, fresh_id, secs};

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
