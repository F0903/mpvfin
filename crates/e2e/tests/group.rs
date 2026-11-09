//! Watch together, end to end: a real mpv in a Jellyfin `SyncPlay` group with a
//! scripted partner, joined and left from mpv's own menu.
//! Run: `cargo test -p e2e --test group -- --ignored --test-threads=1`

use std::thread;
use std::time::{Duration, Instant};

use e2e::{Partner, Player, PlayerOptions, Server, SocketRelay, poll_until};
use jellyfin::{device_name, new_device_id, parse_utc_ms};
use serde_json::{Value, json};

const T10: Duration = Duration::from_secs(10);
const T20: Duration = Duration::from_secs(20);
const T40: Duration = Duration::from_secs(40);
const KEY_SECTION: &str = "mpvfin-skip-key";
/// How close to a commanded position mpv has to be.
const TOLERANCE: f64 = 0.3;

const fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn text(value: &Value) -> String {
    value.as_str().expect("a JSON string").to_owned()
}

fn seconds(ticks: &Value) -> f64 {
    ticks.as_f64().expect("ticks") / 1e7
}

fn utc_ms(value: &Value) -> i64 {
    parse_utc_ms(value.as_str().expect("a timestamp")).expect("a parsable timestamp")
}

/// How long after `received` the command takes effect. `When` and `EmittedAt` are both
/// the server's clock, so this needs no clock sync: `received` is about `EmittedAt`.
fn lead(command: &Value) -> Duration {
    Duration::from_millis(
        u64::try_from(utc_ms(&command["When"]) - utc_ms(&command["EmittedAt"])).unwrap_or(0),
    )
}

/// The position a just-received `Unpause` implies right now.
fn scheduled_position(command: &Value, received: Instant) -> f64 {
    let start = received + lead(command);
    seconds(&command["PositionTicks"])
        + Instant::now()
            .saturating_duration_since(start)
            .as_secs_f64()
}

/// A logged-in mpv and a partner, with no played state or resume left over.
struct Room {
    server: Server,
    player: Player,
    session: String,
    /// Episode ids, E01 first.
    items: Vec<String>,
    partner: Partner,
}

fn room() -> Room {
    room_adjusted(Server::connect(), |_| {})
}

fn room_adjusted(server: Server, adjust: impl FnOnce(&mut PlayerOptions)) -> Room {
    let device = new_device_id();
    let mut opts = PlayerOptions {
        server: server.url().to_owned(),
        state: server.device_login(&device),
        mpv_conf: String::new(),
        config_dir: None,
        default_bindings: false,
        input_conf: String::new(),
    };
    adjust(&mut opts);
    let player = Player::launch(opts);
    // The log line comes after the WebSocket opened; a command sent earlier is lost.
    player.wait_log("connected to Jellyfin as ", T20);
    let session = poll_until(T20, || {
        server
            .mpvfin_session(&device)
            .filter(|s| s["SupportsMediaControl"] == true)
    });
    let session = text(&session["Id"]);
    let items: Vec<String> = server.episodes().into_iter().map(|(id, _)| id).collect();
    for item in &items {
        server.clear_resume(item);
    }
    let partner = Partner::connect(&server);
    Room {
        server,
        player,
        session,
        items,
        partner,
    }
}

/// A room whose plugin reaches the server through a relay the test controls.
fn relayed_room() -> (Room, SocketRelay) {
    let server = Server::connect();
    let relay = SocketRelay::start(&server);
    let r = room_adjusted(server, |opts| {
        relay.url().clone_into(&mut opts.server);
        opts.state.server = Some(relay.url().to_owned());
    });
    (r, relay)
}

/// Picks the entry matching `filter` from the watch-together menu, as a viewer does. There
/// is no uosc under `--vo=null`, so this is mpv's console.
#[track_caller]
fn choose_in_menu(player: &Player, filter: &str) {
    player.command(&["script-binding", "mpvfin/watch-together"]);
    poll_until(T10, || {
        (player.get("user-data/mpv/console/open") == json!(true)).then_some(())
    });
    for key in filter.chars() {
        player.command(&["keypress", &key.to_string()]);
    }
    player.command(&["keypress", "ENTER"]);
}

struct Group {
    id: String,
    name: String,
}

impl Room {
    /// The partner starts a group playing `items` from `start`.
    #[track_caller]
    fn partner_plays(&self, items: &[String], start: Duration) -> Group {
        let name = format!("e2e{}", &new_device_id()[..8]);
        let id = self.partner.create_group(&name);
        let ids: Vec<&str> = items.iter().map(String::as_str).collect();
        self.partner.set_new_queue(&ids, 0, start);
        self.partner.wait_command("Unpause", T10);
        Group { id, name }
    }

    /// mpv joins a group the partner plays, from its menu, and starts with it.
    #[track_caller]
    fn join_playing(&self, items: &[String], start: Duration) -> Group {
        let group = self.partner_plays(items, start);
        self.partner.clear();
        choose_in_menu(&self.player, &group.name);
        self.partner.wait_command("Unpause", T20);
        self.wait_mpv("pause", &json!(false));
        group
    }

    #[track_caller]
    fn wait_mpv(&self, property: &str, expected: &Value) {
        let mut last = Value::Null;
        let reached = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            poll_until(T20, || {
                last = self.player.get(property);
                (last == *expected).then_some(())
            });
        }));
        assert!(
            reached.is_ok(),
            "mpv {property}: expected {expected}, last {last}"
        );
    }

    /// Waits until mpv has `item` open; a group entry's path also names its queue entry.
    #[track_caller]
    fn wait_on(&self, item: &str) {
        let prefix = format!("mpvfin://{item}");
        poll_until(T20, || {
            self.player
                .get("path")
                .as_str()
                .filter(|path| path.starts_with(&prefix))
                .map(str::to_owned)
        });
    }

    /// mpv's position, waiting out the moment a load has none yet.
    #[track_caller]
    fn position(&self) -> f64 {
        poll_until(T10, || self.player.get("time-pos").as_f64())
    }

    /// Waits for mpv to land within `TOLERANCE` of `target`.
    #[track_caller]
    fn wait_near(&self, target: f64) {
        poll_until(secs(8), || {
            self.player
                .get("time-pos")
                .as_f64()
                .filter(|p| (p - target).abs() <= TOLERANCE)
        });
    }

    /// Asserts mpv sits paused at the same position for a while.
    #[track_caller]
    fn assert_stays_paused(&self, how_long: Duration) {
        let before = self.position();
        thread::sleep(how_long);
        assert_eq!(
            self.player.get("pause"),
            json!(true),
            "mpv started by itself"
        );
        let after = self.position();
        assert!((after - before).abs() < 0.05, "moved: {before} to {after}");
    }

    /// Asserts mpv keeps to the group's clock since `command`, received at `received`.
    #[track_caller]
    fn assert_on_schedule(&self, command: &Value, received: Instant, tolerance: f64) {
        let position = self.position();
        let expected = scheduled_position(command, received);
        assert!(
            (position - expected).abs() <= tolerance,
            "mpv at {position}, group at {expected}"
        );
    }

    /// The `PlayQueue` update that put the group on `index`.
    #[track_caller]
    fn wait_queue_at(&self, index: u64) -> Value {
        loop {
            let update = self.partner.wait_update("PlayQueue", T20);
            if update["Data"]["PlayingItemIndex"] == index {
                return update["Data"].clone();
            }
        }
    }

    /// Waits 2 s and asserts no command was emitted after `last`. Older ones are left out:
    /// a `Pause` to the first to report Ready is part of the wait.
    #[track_caller]
    fn assert_quiet_after(&self, last: &Value) {
        thread::sleep(secs(2));
        let extra: Vec<Value> = self
            .partner
            .commands()
            .into_iter()
            .filter(|command| utc_ms(&command["EmittedAt"]) > utc_ms(&last["EmittedAt"]))
            .collect();
        assert!(extra.is_empty(), "the group heard more: {extra:?}");
    }

    fn press_skip_when_up(&self) {
        poll_until(T20, || {
            section_active(&self.player, KEY_SECTION).then_some(())
        });
        self.player.command(&["script-binding", "mpvfin/skip"]);
    }
}

/// Whether `section` is enabled; mpv gives a disabled section priority -1.
fn section_active(player: &Player, section: &str) -> bool {
    player
        .get("input-bindings")
        .as_array()
        .expect("input-bindings")
        .iter()
        .any(|b| b["section"] == section && b["priority"].as_i64().is_some_and(|p| p >= 0))
}

/// The plugin's transport requests since the last call. A repeat in the same direction
/// is invisible to the partner: `Pause` in Paused goes back to its sender alone.
fn transport_posts(relay: &SocketRelay) -> Vec<String> {
    relay
        .take_group_posts()
        .into_iter()
        .filter(|post| ["Pause", "Unpause", "Seek"].contains(&post.as_str()))
        .collect()
}

fn playlist_item_ids(queue: &Value) -> Vec<String> {
    queue["Playlist"]
        .as_array()
        .expect("Playlist")
        .iter()
        .map(|entry| text(&entry["ItemId"]))
        .collect()
}

fn mpv_playlist_ids(player: &Player) -> Vec<String> {
    player
        .get("playlist")
        .as_array()
        .expect("playlist")
        .iter()
        .map(|entry| {
            let filename = text(&entry["filename"]);
            let rest = filename.trim_start_matches("mpvfin://");
            rest.split('/').next().expect("an item id").to_owned()
        })
        .collect()
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn joining_a_playing_group_loads_paused_then_starts_together() {
    let r = room();
    let group = r.partner_plays(&r.items[..1], secs(0));
    thread::sleep(secs(3));

    // A member that never reports Ready holds the group, so the join can be watched.
    let holder = Partner::connect(&r.server);
    holder.set_auto_ready(false);
    r.partner.clear();
    holder.join(&group.id);
    let frozen = seconds(&r.partner.wait_command("Pause", T10)["PositionTicks"]);
    assert!(frozen >= 3.0, "the group had played on: {frozen}");

    choose_in_menu(&r.player, &group.name);
    r.wait_on(&r.items[0]);
    r.wait_near(frozen);
    r.wait_mpv("pause", &json!(true));
    r.assert_stays_paused(Duration::from_millis(1500));

    r.partner.clear();
    holder.leave();
    let unpause = r.partner.wait_command("Unpause", T10);
    let received = Instant::now();
    r.wait_mpv("pause", &json!(false));
    thread::sleep(secs(1));
    r.assert_on_schedule(&unpause, received, 1.0);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn creating_a_group_while_playing_publishes_the_queue() {
    let r = room();
    let ids: Vec<&str> = r.items.iter().map(String::as_str).collect();
    r.server.play(&r.session, &ids[..2], 0, secs(30));
    r.wait_on(ids[0]);
    r.wait_mpv("playlist-count", &json!(2));

    let before_create = r.position();
    choose_in_menu(&r.player, "create");
    let group = poll_until(T10, || {
        r.partner.groups().into_iter().find(|g| {
            g["GroupName"] == device_name("mpvfin") && g["Participants"] == json!(["admin"])
        })
    });

    r.partner.join(&text(&group["GroupId"]));
    let queue = r.wait_queue_at(0);
    assert_eq!(playlist_item_ids(&queue), &ids[..2]);
    let published = seconds(&queue["StartPositionTicks"]);
    assert!(
        (before_create.max(30.0)..50.0).contains(&published),
        "the group must start where mpv was, not at {published}"
    );

    let unpause = r.partner.wait_command("Unpause", T20);
    let received = Instant::now();
    r.wait_mpv("pause", &json!(false));
    r.wait_on(ids[0]);
    assert_eq!(mpv_playlist_ids(&r.player), &ids[..2]);
    thread::sleep(secs(1));
    r.assert_on_schedule(&unpause, received, 1.0);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn partner_joining_pauses_and_resumes_mpv() {
    let r = room();
    let group = r.join_playing(&r.items[..1], secs(0));

    let second = Partner::connect(&r.server);
    second.set_auto_ready(false);
    second.join(&group.id);
    r.wait_mpv("pause", &json!(true));
    r.assert_stays_paused(Duration::from_millis(1500));

    let queue = poll_until(T10, || Some(second.last_queue()).filter(|q| !q.is_null()));
    let pid = text(&queue["Playlist"][0]["PlaylistItemId"]);
    let position = Duration::from_secs_f64(seconds(&queue["StartPositionTicks"]));
    r.partner.clear();
    second.ready(&pid, position);
    let unpause = r.partner.wait_command("Unpause", T10);
    let received = Instant::now();
    r.wait_mpv("pause", &json!(false));
    thread::sleep(secs(1));
    r.assert_on_schedule(&unpause, received, TOLERANCE);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn partner_pause_seek_and_resume_are_followed() {
    let r = room();
    r.join_playing(&r.items[..1], secs(0));
    thread::sleep(secs(2));

    r.partner.clear();
    r.partner.pause();
    let pause = r.partner.wait_command("Pause", T10);
    r.wait_mpv("pause", &json!(true));
    r.wait_near(seconds(&pause["PositionTicks"]));

    // Seeking a paused group: the Ready of both brings a Pause back.
    r.partner.clear();
    r.partner.seek(secs(60));
    r.wait_near(60.0);
    r.partner.wait_command("Pause", T10);
    r.wait_near(60.0);
    r.assert_stays_paused(secs(1));

    r.partner.clear();
    r.partner.unpause();
    let unpause = r.partner.wait_command("Unpause", T10);
    let received = Instant::now();
    r.wait_mpv("pause", &json!(false));
    thread::sleep(secs(1));
    r.assert_on_schedule(&unpause, received, TOLERANCE);

    // Seeking a playing group, backwards: it stops for everyone's Ready and restarts.
    r.partner.clear();
    r.partner.seek(secs(30));
    let unpause = r.partner.wait_command("Unpause", T10);
    let received = Instant::now();
    r.wait_mpv("pause", &json!(false));
    thread::sleep(secs(1));
    r.assert_on_schedule(&unpause, received, TOLERANCE);
    assert!(r.position() < 35.0, "the seek back was not followed");
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn unpause_starts_at_the_scheduled_time() {
    let r = room();
    r.join_playing(&r.items[..1], secs(0));
    r.partner.clear();
    r.partner.pause();
    r.partner.wait_command("Pause", T10);
    r.wait_mpv("pause", &json!(true));

    r.partner.clear();
    r.partner.unpause();
    let unpause = r.partner.wait_command("Unpause", T10);
    let received = Instant::now();
    let lead = lead(&unpause);
    assert!(
        lead >= Duration::from_millis(300),
        "too short to tell: {lead:?}"
    );

    let started = poll_until(T10, || {
        (r.player.get("pause") == json!(false)).then(|| received.elapsed())
    });
    let early = lead.saturating_sub(started);
    let late = started.saturating_sub(lead);
    assert!(
        early <= Duration::from_millis(150) && late <= Duration::from_millis(300),
        "started {started:?} after the broadcast, due {lead:?}"
    );
    let commanded = seconds(&unpause["PositionTicks"]);
    assert!(
        (r.position() - commanded).abs() <= TOLERANCE,
        "must start from the commanded position, {commanded}"
    );
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn local_pause_and_seek_reach_the_group() {
    let (r, relay) = relayed_room();
    r.join_playing(&r.items[..1], secs(0));
    thread::sleep(secs(2));

    relay.take_group_posts();
    r.partner.clear();
    r.player.command(&["set", "pause", "yes"]);
    let pause = r.partner.wait_command("Pause", T10);
    let at = seconds(&pause["PositionTicks"]);
    assert!((r.position() - at).abs() <= 0.5, "paused away from {at}");
    r.assert_quiet_after(&pause);
    assert_eq!(transport_posts(&relay), ["Pause"], "one request per action");
    assert_eq!(
        r.player.get("pause"),
        json!(true),
        "the echo must not resume"
    );

    // Paused: the Seek comes back to both, and their Ready leaves the group Paused.
    r.partner.clear();
    r.player.command(&["seek", "40", "absolute+exact"]);
    let seek = r.partner.wait_command("Seek", T10);
    let at = seconds(&seek["PositionTicks"]);
    assert!((at - 40.0).abs() <= 0.3, "the group sought to {at}");
    let pause = r.partner.wait_command("Pause", T10);
    r.assert_quiet_after(&pause);
    assert_eq!(transport_posts(&relay), ["Seek"], "one request per action");
    assert_eq!(r.player.get("pause"), json!(true));
    r.wait_near(40.0);

    r.partner.clear();
    r.player.command(&["set", "pause", "no"]);
    let unpause = r.partner.wait_command("Unpause", T10);
    let received = Instant::now();
    r.wait_mpv("pause", &json!(false));
    r.assert_quiet_after(&unpause);
    assert_eq!(
        transport_posts(&relay),
        ["Unpause"],
        "one request per action"
    );
    assert_eq!(r.player.get("pause"), json!(false));
    r.assert_on_schedule(&unpause, received, TOLERANCE);

    // Playing: the seek stops the group for both Readys, then it resumes once.
    r.partner.clear();
    r.player.command(&["seek", "70", "absolute+exact"]);
    let seek = r.partner.wait_command("Seek", T10);
    assert!((seconds(&seek["PositionTicks"]) - 70.0).abs() <= 0.3);
    let unpause = r.partner.wait_command("Unpause", T10);
    r.assert_quiet_after(&unpause);
    assert_eq!(transport_posts(&relay), ["Seek"], "one request per action");
    assert_eq!(r.player.get("pause"), json!(false));
    assert!(r.position() > 70.0, "the seek did not stick");
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn next_and_previous_in_mpv_move_the_group() {
    let r = room();
    r.join_playing(&r.items, secs(0));
    r.wait_mpv("playlist-count", &json!(3));

    r.partner.clear();
    r.player.command(&["playlist-next"]);
    let queue = r.wait_queue_at(1);
    assert_eq!(
        queue["Playlist"][1]["ItemId"], r.items[1],
        "the group is on mpv's next entry"
    );
    r.partner.wait_command("Unpause", T20);
    r.wait_on(&r.items[1]);
    r.wait_mpv("pause", &json!(false));
    r.wait_mpv("playlist-pos", &json!(1));

    r.partner.clear();
    r.player.command(&["playlist-prev"]);
    r.wait_queue_at(0);
    r.partner.wait_command("Unpause", T20);
    r.wait_on(&r.items[0]);
    r.wait_mpv("pause", &json!(false));
    r.wait_mpv("playlist-pos", &json!(0));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn cast_while_grouped_replaces_the_group_queue() {
    let r = room();
    r.join_playing(&r.items[..1], secs(0));

    r.partner.clear();
    r.server.play(&r.session, &[&r.items[1]], 0, secs(15));
    let update = r.partner.wait_update("PlayQueue", T20);
    let queue = &update["Data"];
    assert_eq!(queue["Reason"], "NewPlaylist");
    let index = usize::try_from(queue["PlayingItemIndex"].as_u64().expect("index")).expect("index");
    assert_eq!(queue["Playlist"][index]["ItemId"], r.items[1]);
    assert!((seconds(&queue["StartPositionTicks"]) - 15.0).abs() < 1e-6);

    let unpause = r.partner.wait_command("Unpause", T20);
    let received = Instant::now();
    r.wait_on(&r.items[1]);
    r.wait_mpv("pause", &json!(false));
    assert_eq!(mpv_playlist_ids(&r.player), playlist_item_ids(queue));
    thread::sleep(secs(1));
    r.assert_on_schedule(&unpause, received, TOLERANCE);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn end_of_item_holds_and_requests_next() {
    let (r, relay) = relayed_room();
    r.join_playing(&r.items[..2], secs(0));
    r.partner.clear();
    r.partner.seek(secs(114));
    r.partner.wait_command("Unpause", T10);
    // mpv's own copy of the Unpause has to be through before the socket is held.
    r.wait_mpv("pause", &json!(false));

    // The partner stops answering, so the group waits on it for the next item.
    r.partner.set_auto_ready(false);
    r.partner.clear();
    // With the server's answer held back, only mpv's own behaviour can move it on.
    relay.hold_server_messages(true);
    let queue = r.wait_queue_at(1);
    assert_eq!(
        queue["Reason"], "NextItem",
        "mpv must ask the server, not move on by itself"
    );
    r.wait_mpv("eof-reached", &json!(true));
    thread::sleep(Duration::from_millis(1500));
    r.wait_on(&r.items[0]);
    assert_eq!(r.player.get("playlist-pos"), json!(0), "mpv did not hold");

    relay.hold_server_messages(false);
    r.wait_on(&r.items[1]);
    r.wait_mpv("pause", &json!(true));
    r.assert_stays_paused(Duration::from_millis(1500));
    assert!(r.position() < 1.0, "the next item must start from the top");

    r.partner.clear();
    r.partner.ready(
        &text(&queue["Playlist"][1]["PlaylistItemId"]),
        Duration::ZERO,
    );
    r.partner.wait_command("Unpause", T10);
    r.wait_mpv("pause", &json!(false));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn end_of_queue_stops_the_group() {
    let r = room();
    r.join_playing(&r.items[2..], secs(0));
    r.partner.clear();
    r.partner.seek(secs(115));
    r.partner.wait_command("Unpause", T10);

    r.partner.clear();
    r.partner.wait_command("Stop", T20);
    r.wait_mpv("idle-active", &json!(true));
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn skip_in_a_group_seeks_everyone() {
    // The button's key section only reports active with mpv's default bindings on.
    let r = room_adjusted(Server::connect(), |opts| opts.default_bindings = true);
    r.join_playing(&r.items[..1], secs(7));
    assert!(r.position() < 18.0, "the intro is nearly over");

    r.partner.clear();
    r.press_skip_when_up();
    let seek = r.partner.wait_command("Seek", T10);
    let at = seconds(&seek["PositionTicks"]);
    assert!((at - 20.0).abs() <= 0.5, "the group sought to {at}");
    r.partner.wait_command("Unpause", T10);
    r.wait_mpv("pause", &json!(false));
    assert!(r.position() >= 19.5, "mpv stayed behind: {}", r.position());
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn play_now_moves_the_whole_group() {
    // The card's key section only reports active with mpv's default bindings on.
    let r = room_adjusted(Server::connect(), |opts| opts.default_bindings = true);
    // In E01's credits.
    r.join_playing(&r.items[..2], secs(103));
    r.wait_mpv("playlist-count", &json!(2));
    poll_until(T10, || {
        section_active(&r.player, "mpvfin-next-up-key").then_some(())
    });

    r.partner.clear();
    r.player.command(&["script-binding", "mpvfin/next-up"]);
    let queue = r.wait_queue_at(1);
    assert_eq!(queue["Playlist"][1]["ItemId"], r.items[1]);
    r.partner.wait_command("Unpause", T20);
    r.wait_on(&r.items[1]);
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn leaving_keeps_local_playback() {
    let (r, relay) = relayed_room();
    r.join_playing(&r.items[..1], secs(0));
    assert_eq!(r.player.get("keep-open"), json!("always"));

    r.partner.clear();
    choose_in_menu(&r.player, "leave");
    r.partner.wait_update("UserLeft", T10);
    r.wait_on(&r.items[0]);
    r.wait_mpv("keep-open", &json!(false));
    let before = r.position();
    thread::sleep(secs(1));
    assert_eq!(r.player.get("pause"), json!(false));
    assert!(r.position() > before, "playback must carry on");

    // A rejoin guard only: no broadcast reaches a non-member, whatever the plugin does.
    r.partner.clear();
    r.partner.pause();
    r.partner.wait_command("Pause", T10);
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        r.player.get("pause"),
        json!(false),
        "mpv rejoined the group after leaving"
    );

    // The server would answer a non-member's request to it alone, so look at the wire.
    relay.take_group_posts();
    r.player.command(&["set", "pause", "yes"]);
    thread::sleep(secs(2));
    assert_eq!(
        relay.take_group_posts(),
        Vec::<String>::new(),
        "a viewer who left must not ask the group for anything"
    );
}

#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn local_file_leaves_the_group() {
    let r = room();
    r.join_playing(&r.items[..1], secs(0));

    let local = "av://lavfi:testsrc=size=64x64:rate=1";
    r.partner.clear();
    r.player.command(&["loadfile", local, "replace"]);
    r.partner.wait_update("UserLeft", T10);
    r.wait_mpv("path", &json!(local));
    r.wait_mpv("pause", &json!(false));

    // A rejoin guard only: no broadcast reaches a non-member, whatever the plugin does.
    r.partner.clear();
    r.partner.pause();
    r.partner.wait_command("Pause", T10);
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        r.player.get("pause"),
        json!(false),
        "mpv rejoined the group after leaving"
    );
}

/// A real socket drop: the plugin connects through a relay that cuts its WebSocket.
#[test]
#[ignore = "needs ./scripts/test-server.ps1 -Action up"]
fn reconnect_rejoins_the_group() {
    let (r, relay) = relayed_room();
    r.join_playing(&r.items[..1], secs(0));

    r.partner.clear();
    assert_eq!(relay.cut_sockets(), 1, "the plugin's WebSocket");
    r.partner.wait_update("UserLeft", T20);
    r.partner.wait_update("UserJoined", T40);
    let unpause = r.partner.wait_command("Unpause", T20);
    let received = Instant::now();
    r.wait_mpv("pause", &json!(false));
    thread::sleep(secs(1));
    r.assert_on_schedule(&unpause, received, TOLERANCE);

    // Still a real member.
    r.partner.clear();
    r.partner.pause();
    r.partner.wait_command("Pause", T10);
    r.wait_mpv("pause", &json!(true));
}
