//! The skip button: when it shows, and how a key or click skips.

use std::time::Duration;

use e2e::poll_until;
use serde_json::json;

use crate::{CLICK_SECTION, KEY_SECTION, T10, secs, section_active, skip_cast};

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
