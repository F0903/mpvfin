//! The Next Up card: when it shows, what it says, and Play Now.

use std::time::Duration;

use e2e::poll_until;
use serde_json::json;

use crate::{
    KEY_SECTION, NEXT_CLICK_SECTION, NEXT_KEY_SECTION, T10, T20, secs, section_active, skip_cast,
};

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
