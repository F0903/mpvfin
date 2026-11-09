//! End-to-end casting tests against the Docker Jellyfin and a real mpv.
//! Run: `cargo test -p e2e -- --ignored --test-threads=1`

mod lifecycle;
mod login;
mod next_up;
mod remote;
mod skip;

use std::path::PathBuf;
use std::time::Duration;

use e2e::{Player, PlayerOptions, Server, State, poll_until, reported_position};
use serde_json::Value;

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
    jellyfin::new_device_id()
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
