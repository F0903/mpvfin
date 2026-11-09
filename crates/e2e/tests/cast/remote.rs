//! Remote control from a Jellyfin client, and what mpv reports back.

use std::time::Duration;

use e2e::{poll_until, reported_position};
use serde_json::json;

use crate::{T20, cast, secs};

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
