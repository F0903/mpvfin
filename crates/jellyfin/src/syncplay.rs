//! `SyncPlay` wire format: inbound group messages and outbound requests.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::client::{Client, HttpError};
use crate::wire::{duration_to_ticks, ticks_to_duration};

/// The entry id the server sends when a group has no current item.
pub const ZERO_ID: &str = "00000000000000000000000000000000";

/// One entry of a `SyncPlay` queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupEntry {
    /// Library item id.
    pub item_id: String,
    /// The queue entry's own id, stable for its lifetime.
    pub playlist_item_id: String,
}

/// Why the group's queue update was sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueReason {
    /// A whole new queue.
    NewPlaylist,
    /// The current item was set.
    SetCurrentItem,
    /// Moved to the next item.
    NextItem,
    /// Moved to the previous item.
    PreviousItem,
    /// Items were removed.
    RemoveItems,
    /// Anything that does not change the playing item.
    Other,
}

/// A group's play queue as the server last described it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupQueue {
    /// Why it was sent.
    pub reason: QueueReason,
    /// Server time of the update, in ms since the Unix epoch.
    pub last_update_ms: i64,
    /// The queue in server order.
    pub entries: Vec<GroupEntry>,
    /// Index of the current entry, if any.
    pub playing_index: Option<usize>,
    /// Where to start (or the extrapolated position).
    pub start_position: Duration,
    /// Whether the group is playing.
    pub is_playing: bool,
}

/// A group playback command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupCommandKind {
    /// Start playing.
    Unpause,
    /// Pause.
    Pause,
    /// Seek.
    Seek,
    /// Stop.
    Stop,
}

/// A group playback command with its timing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupCommand {
    /// What to do.
    pub kind: GroupCommandKind,
    /// Entry it applies to; all zeros when none.
    pub playlist_item_id: String,
    /// Position at `When`.
    pub position: Duration,
    /// `When` minus server now at receipt; may be negative.
    pub delay_ms: i64,
}

/// One group in the server's list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupSummary {
    /// Group id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Distinct participants.
    pub members: usize,
    /// Server state name.
    pub state: String,
}

/// A request a member sends to the group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupRequest {
    /// Create a group.
    New {
        /// Group name.
        name: String,
    },
    /// Join a group.
    Join {
        /// Group id.
        group_id: String,
    },
    /// Leave the group.
    Leave,
    /// Replace the queue.
    SetNewQueue {
        /// Items in order.
        item_ids: Vec<String>,
        /// Index to play.
        index: usize,
        /// Start position.
        start: Duration,
    },
    /// Add to the queue.
    Queue {
        /// Items to add.
        item_ids: Vec<String>,
        /// Insert after the current item instead of at the end.
        next: bool,
    },
    /// Jump to an entry.
    SetPlaylistItem {
        /// Entry id.
        pid: String,
    },
    /// Advance from an entry.
    NextItem {
        /// Entry id.
        pid: String,
    },
    /// Go back from an entry.
    PreviousItem {
        /// Entry id.
        pid: String,
    },
    /// Resume the group.
    Unpause,
    /// Pause the group.
    Pause,
    /// Stop the group.
    Stop,
    /// Seek the group.
    Seek {
        /// Target position.
        position: Duration,
    },
    /// Report ready at a position.
    Ready {
        /// Sampled position.
        position: Duration,
        /// Whether local playback is running.
        is_playing: bool,
        /// Entry id.
        pid: String,
    },
    /// Report buffering at a position.
    Buffering {
        /// Sampled position.
        position: Duration,
        /// Entry id.
        pid: String,
    },
}

/// What a `SyncPlay` message means to the group session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupUpdate {
    /// We are now in this group.
    Joined {
        /// Group id.
        group_id: String,
    },
    /// We left the group.
    Left,
    /// The group is unusable; show this message.
    Gone {
        /// User-facing reason.
        message: &'static str,
    },
    /// A queue update.
    Queue(GroupQueue),
    /// A playback command; `delay_ms` is left 0 for the session to fill from
    /// `when_ms` (server time) and its clock.
    Command {
        /// The command.
        command: GroupCommand,
        /// `When`, in ms since the Unix epoch, server time.
        when_ms: i64,
    },
    /// Informational or unknown.
    Ignored,
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.f...]Z` to ms since the epoch; extra
/// fractional digits are truncated. Never panics.
#[must_use]
pub fn parse_utc_ms(text: &str) -> Option<i64> {
    let b = text.strip_suffix('Z')?.as_bytes();
    if b.len() < 19 || !b.is_ascii() {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let digits = &b[r];
        if digits.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(digits).ok()?.parse().ok()
        } else {
            None
        }
    };
    if [b[4], b[7], b[10], b[13], b[16]] != *b"--T::" {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let frac = match &b[19..] {
        [] => 0,
        [b'.', digits @ ..] if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) => {
            let mut ms = 0;
            for i in 0..3 {
                ms = ms * 10 + i64::from(digits.get(i).map_or(0, |c| c - b'0'));
            }
            ms
        }
        _ => return None,
    };
    let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec;
    Some(secs * 1000 + frac)
}

/// Formats ms since the epoch as UTC with millisecond precision and `Z`.
#[must_use]
pub fn format_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60,
        ms.rem_euclid(1000)
    )
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn ticks(v: &Value, key: &str) -> Duration {
    ticks_to_duration(v.get(key).and_then(Value::as_i64).unwrap_or(0))
}

fn parse_command(data: &Value) -> Option<GroupUpdate> {
    let kind = match text(data, "Command")? {
        "Unpause" => GroupCommandKind::Unpause,
        "Pause" => GroupCommandKind::Pause,
        "Seek" => GroupCommandKind::Seek,
        "Stop" => GroupCommandKind::Stop,
        _ => return None,
    };
    Some(GroupUpdate::Command {
        command: GroupCommand {
            kind,
            playlist_item_id: text(data, "PlaylistItemId").unwrap_or(ZERO_ID).to_owned(),
            position: ticks(data, "PositionTicks"),
            delay_ms: 0,
        },
        when_ms: parse_utc_ms(text(data, "When")?)?,
    })
}

fn parse_queue(data: &Value) -> Option<GroupUpdate> {
    let reason = match text(data, "Reason")? {
        "NewPlaylist" => QueueReason::NewPlaylist,
        "SetCurrentItem" => QueueReason::SetCurrentItem,
        "NextItem" => QueueReason::NextItem,
        "PreviousItem" => QueueReason::PreviousItem,
        "RemoveItems" => QueueReason::RemoveItems,
        _ => QueueReason::Other,
    };
    let entries = data
        .get("Playlist")?
        .as_array()?
        .iter()
        .filter_map(|e| {
            Some(GroupEntry {
                item_id: text(e, "ItemId")?.to_owned(),
                playlist_item_id: text(e, "PlaylistItemId")?.to_owned(),
            })
        })
        .collect();
    Some(GroupUpdate::Queue(GroupQueue {
        reason,
        last_update_ms: parse_utc_ms(text(data, "LastUpdate")?)?,
        entries,
        playing_index: data
            .get("PlayingItemIndex")
            .and_then(Value::as_i64)
            .and_then(|i| usize::try_from(i).ok()),
        start_position: ticks(data, "StartPositionTicks"),
        is_playing: data
            .get("IsPlaying")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }))
}

/// Maps a `SyncPlayCommand` or `SyncPlayGroupUpdate` message to an update.
/// Never panics; anything unusable is `Ignored`.
#[must_use]
pub fn parse_group_message(message_type: &str, data: &Value) -> GroupUpdate {
    let parsed = match message_type {
        "SyncPlayCommand" => parse_command(data),
        "SyncPlayGroupUpdate" => {
            let inner = data.get("Data").unwrap_or(&Value::Null);
            match text(data, "Type") {
                Some("GroupJoined") => text(data, "GroupId").map(|g| GroupUpdate::Joined {
                    group_id: g.to_owned(),
                }),
                Some("GroupLeft") => Some(GroupUpdate::Left),
                Some("GroupDoesNotExist") => Some(GroupUpdate::Gone {
                    message: "That group no longer exists",
                }),
                Some("NotInGroup") => Some(GroupUpdate::Gone {
                    message: "You are no longer in that group",
                }),
                Some("LibraryAccessDenied") => Some(GroupUpdate::Gone {
                    message: "You don't have access to everything in that group",
                }),
                Some("PlayQueue") => parse_queue(inner),
                _ => None,
            }
        }
        _ => None,
    };
    parsed.unwrap_or(GroupUpdate::Ignored)
}

/// The HTTP path (with the `/SyncPlay/` prefix) and JSON body for a request.
/// `server_now_ms` stamps `Ready` and `Buffering`. Bodiless requests give
/// `Value::Null`.
#[must_use]
pub fn request_path_and_body(request: &GroupRequest, server_now_ms: i64) -> (&'static str, Value) {
    let report = |position: &Duration, is_playing: bool, pid: &str| {
        json!({
            "When": format_utc(server_now_ms),
            "PositionTicks": duration_to_ticks(*position),
            "IsPlaying": is_playing,
            "PlaylistItemId": pid,
        })
    };
    let pid_body = |pid: &str| json!({ "PlaylistItemId": pid });
    match request {
        GroupRequest::New { name } => ("/SyncPlay/New", json!({ "GroupName": name })),
        GroupRequest::Join { group_id } => ("/SyncPlay/Join", json!({ "GroupId": group_id })),
        GroupRequest::Leave => ("/SyncPlay/Leave", Value::Null),
        GroupRequest::SetNewQueue {
            item_ids,
            index,
            start,
        } => (
            "/SyncPlay/SetNewQueue",
            json!({
                "PlayingQueue": item_ids,
                "PlayingItemPosition": index,
                "StartPositionTicks": duration_to_ticks(*start),
            }),
        ),
        GroupRequest::Queue { item_ids, next } => (
            "/SyncPlay/Queue",
            json!({
                "ItemIds": item_ids,
                "Mode": if *next { "QueueNext" } else { "Queue" },
            }),
        ),
        GroupRequest::SetPlaylistItem { pid } => ("/SyncPlay/SetPlaylistItem", pid_body(pid)),
        GroupRequest::NextItem { pid } => ("/SyncPlay/NextItem", pid_body(pid)),
        GroupRequest::PreviousItem { pid } => ("/SyncPlay/PreviousItem", pid_body(pid)),
        GroupRequest::Unpause => ("/SyncPlay/Unpause", Value::Null),
        GroupRequest::Pause => ("/SyncPlay/Pause", Value::Null),
        GroupRequest::Stop => ("/SyncPlay/Stop", Value::Null),
        GroupRequest::Seek { position } => (
            "/SyncPlay/Seek",
            json!({ "PositionTicks": duration_to_ticks(*position) }),
        ),
        GroupRequest::Ready {
            position,
            is_playing,
            pid,
        } => ("/SyncPlay/Ready", report(position, *is_playing, pid)),
        GroupRequest::Buffering { position, pid } => {
            ("/SyncPlay/Buffering", report(position, true, pid))
        }
    }
}

/// Parses the `GET /SyncPlay/List` body, skipping malformed entries.
#[must_use]
pub fn parse_groups(body: &Value) -> Vec<GroupSummary> {
    let Some(list) = body.as_array() else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|g| {
            Some(GroupSummary {
                id: text(g, "GroupId")?.to_owned(),
                name: text(g, "GroupName")?.to_owned(),
                members: g.get("Participants")?.as_array()?.len(),
                state: text(g, "State").unwrap_or("").to_owned(),
            })
        })
        .collect()
}

/// NTP-style `(offset, rtt)` in ms from local send `t0`, server receive `t1`,
/// server send `t2` and local receive `t3`.
#[must_use]
pub fn clock_sample(t0_ms: i64, t1_ms: i64, t2_ms: i64, t3_ms: i64) -> (i64, i64) {
    (
        i64::midpoint(t1_ms - t0_ms, t2_ms - t3_ms),
        (t3_ms - t0_ms) - (t2_ms - t1_ms),
    )
}

/// Milliseconds since the Unix epoch by the local clock, the one clock samples measure.
#[must_use]
pub fn local_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl Client {
    /// Sends a group request, stamped with the server's time now.
    ///
    /// # Errors
    /// The request failed.
    pub fn syncplay(&self, request: &GroupRequest, server_now_ms: i64) -> Result<(), HttpError> {
        let (path, body) = request_path_and_body(request, server_now_ms);
        self.post(path, &body).map(drop)
    }

    /// The server's groups.
    ///
    /// # Errors
    /// The request failed.
    pub fn list_groups(&self) -> Result<Vec<GroupSummary>, HttpError> {
        Ok(parse_groups(&self.get("/SyncPlay/List")?))
    }

    /// One `/GetUtcTime` round trip as `(offset, rtt)` in ms; see [`clock_sample`].
    ///
    /// # Errors
    /// The request failed, or the reply has no times.
    pub fn clock_probe(&self) -> Result<(i64, i64), HttpError> {
        let t0 = local_now_ms();
        let reply = self.get("/GetUtcTime")?;
        let t3 = local_now_ms();
        let stamp = |key| {
            reply
                .get(key)
                .and_then(Value::as_str)
                .and_then(parse_utc_ms)
        };
        match (
            stamp("RequestReceptionTime"),
            stamp("ResponseTransmissionTime"),
        ) {
            (Some(t1), Some(t2)) => Ok(clock_sample(t0, t1, t2, t3)),
            _ => Err(HttpError::transport("bad /GetUtcTime reply")),
        }
    }

    /// Tells the group this client's one-way delay; the server doubles it for its start delay.
    ///
    /// # Errors
    /// The request failed.
    pub fn ping(&self, one_way_ms: i64) -> Result<(), HttpError> {
        self.post("/SyncPlay/Ping", &json!({ "Ping": one_way_ms }))
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(kind: &str, data: &Value) -> GroupUpdate {
        parse_group_message(
            "SyncPlayGroupUpdate",
            &json!({"GroupId": "g1", "Type": kind, "Data": data}),
        )
    }

    fn queue_of(u: GroupUpdate) -> GroupQueue {
        match u {
            GroupUpdate::Queue(q) => q,
            other => panic!("not a queue: {other:?}"),
        }
    }

    #[test]
    fn parses_every_update_type() {
        assert_eq!(
            group("GroupJoined", &json!({"GroupName": "n"})),
            GroupUpdate::Joined {
                group_id: "g1".into()
            }
        );
        assert_eq!(group("GroupLeft", &json!("g-1")), GroupUpdate::Left);
        for (t, m) in [
            ("GroupDoesNotExist", "That group no longer exists"),
            ("NotInGroup", "You are no longer in that group"),
            (
                "LibraryAccessDenied",
                "You don't have access to everything in that group",
            ),
        ] {
            assert_eq!(group(t, &json!("")), GroupUpdate::Gone { message: m });
        }
        for t in ["UserJoined", "UserLeft", "StateUpdate", "Bogus"] {
            assert_eq!(group(t, &json!("x")), GroupUpdate::Ignored);
        }
        assert_eq!(
            parse_group_message("Other", &Value::Null),
            GroupUpdate::Ignored
        );
        assert_eq!(
            parse_group_message("SyncPlayGroupUpdate", &json!(5)),
            GroupUpdate::Ignored
        );
    }

    fn queue_json(reason: &str, index: i64) -> Value {
        json!({
            "Reason": reason, "LastUpdate": "2026-01-02T03:04:05.5Z",
            "Playlist": [
                {"ItemId": "i1", "PlaylistItemId": "p1"},
                {"ItemId": "bad"},
                {"ItemId": "i2", "PlaylistItemId": "p2"}],
            "PlayingItemIndex": index, "StartPositionTicks": 20_000_000, "IsPlaying": true
        })
    }

    #[test]
    fn parses_play_queue_with_no_current_item() {
        let q = queue_of(group("PlayQueue", &queue_json("RemoveItems", -1)));
        assert_eq!(q.reason, QueueReason::RemoveItems);
        assert_eq!(q.playing_index, None);
        assert_eq!(q.entries.len(), 2);
        assert_eq!(q.entries[1].playlist_item_id, "p2");
        assert_eq!(q.start_position, Duration::from_secs(2));
        assert!(q.is_playing);
        assert_eq!(
            Some(q.last_update_ms),
            parse_utc_ms("2026-01-02T03:04:05.500Z")
        );

        let q = queue_of(group("PlayQueue", &queue_json("NewPlaylist", 1)));
        assert_eq!(
            (q.reason, q.playing_index),
            (QueueReason::NewPlaylist, Some(1))
        );
        for (r, want) in [
            ("SetCurrentItem", QueueReason::SetCurrentItem),
            ("NextItem", QueueReason::NextItem),
            ("PreviousItem", QueueReason::PreviousItem),
            ("Queue", QueueReason::Other),
            ("QueueNext", QueueReason::Other),
            ("MoveItem", QueueReason::Other),
            ("RepeatMode", QueueReason::Other),
            ("ShuffleMode", QueueReason::Other),
        ] {
            let q = queue_of(group("PlayQueue", &queue_json(r, 0)));
            assert_eq!(q.reason, want, "{r}");
        }
        assert_eq!(
            group("PlayQueue", &json!({"Reason": "NewPlaylist"})),
            GroupUpdate::Ignored
        );
    }

    #[test]
    fn parses_commands() {
        let msg = |c: &str, pid: Value| {
            parse_group_message(
                "SyncPlayCommand",
                &json!({"GroupId": "g", "PlaylistItemId": pid, "When": "2026-01-02T03:04:05.25Z",
                        "PositionTicks": 50_000_000, "Command": c, "EmittedAt": "x"}),
            )
        };
        let when_ms = parse_utc_ms("2026-01-02T03:04:05.250Z").unwrap_or_default();
        for (c, kind) in [
            ("Unpause", GroupCommandKind::Unpause),
            ("Pause", GroupCommandKind::Pause),
            ("Seek", GroupCommandKind::Seek),
            ("Stop", GroupCommandKind::Stop),
        ] {
            assert_eq!(
                msg(c, json!("p9")),
                GroupUpdate::Command {
                    command: GroupCommand {
                        kind,
                        playlist_item_id: "p9".into(),
                        position: Duration::from_secs(5),
                        delay_ms: 0
                    },
                    when_ms
                }
            );
        }
        match msg("Stop", Value::Null) {
            GroupUpdate::Command { command, .. } => assert_eq!(command.playlist_item_id, ZERO_ID),
            other => panic!("not a command: {other:?}"),
        }
        assert_eq!(msg("Rewind", json!("p")), GroupUpdate::Ignored);
    }

    #[test]
    fn parse_utc_handles_variable_fractions() {
        let base = 1_767_323_045_000; // 2026-01-02T03:04:05Z
        assert_eq!(parse_utc_ms("2026-01-02T03:04:05Z"), Some(base));
        assert_eq!(parse_utc_ms("2026-01-02T03:04:05.5Z"), Some(base + 500));
        assert_eq!(parse_utc_ms("2026-01-02T03:04:05.957Z"), Some(base + 957));
        assert_eq!(
            parse_utc_ms("2026-01-02T03:04:05.941608Z"),
            Some(base + 941)
        );
        assert_eq!(
            parse_utc_ms("2026-01-02T03:04:05.3987244Z"),
            Some(base + 398)
        );
        assert_eq!(parse_utc_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc_ms("2000-02-29T00:00:00Z"), Some(951_782_400_000));
    }

    #[test]
    fn parse_utc_rejects_garbage() {
        for s in [
            "",
            "Z",
            "garbage",
            "2026-01-02T03:04:05",
            "2026-01-02 03:04:05Z",
            "2026-13-02T03:04:05Z",
            "2026-01-02T25:04:05Z",
            "2026-01-02T03:04:05.Z",
            "2026-01-02T03:04:05.1xZ",
            "2026-01-02T03:04:05+01:00Z",
            "20\u{e9}6-01-02T03:04:05Z",
            "+026-01-02T03:04:05Z",
            "2026-01-02T03:04:05.5ZZ",
        ] {
            assert_eq!(parse_utc_ms(s), None, "{s:?}");
        }
    }

    #[test]
    fn format_utc_round_trips() {
        assert_eq!(format_utc(1_767_323_045_007), "2026-01-02T03:04:05.007Z");
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        for ms in [
            0,
            1,
            951_782_400_123,
            1_767_323_045_999,
            4_102_444_800_000,
            86_400_000,
        ] {
            assert_eq!(parse_utc_ms(&format_utc(ms)), Some(ms), "{ms}");
        }
    }

    #[test]
    fn every_request_maps_to_its_path_and_body() {
        use GroupRequest as R;
        let s = |x: &str| x.to_owned();
        let pos = Duration::from_secs(3);
        let now = 1_767_323_045_007;
        let when = "2026-01-02T03:04:05.007Z";
        let ready = |playing: bool| json!({"When": when, "PositionTicks": 30_000_000, "IsPlaying": playing, "PlaylistItemId": "p"});
        let pid = json!({"PlaylistItemId": "p"});
        let table: Vec<(R, &str, Value)> = vec![
            (
                R::New { name: s("n") },
                "/SyncPlay/New",
                json!({"GroupName": "n"}),
            ),
            (
                R::Join { group_id: s("g") },
                "/SyncPlay/Join",
                json!({"GroupId": "g"}),
            ),
            (R::Leave, "/SyncPlay/Leave", Value::Null),
            (
                R::SetNewQueue {
                    item_ids: vec![s("a"), s("b")],
                    index: 1,
                    start: pos,
                },
                "/SyncPlay/SetNewQueue",
                json!({"PlayingQueue": ["a", "b"], "PlayingItemPosition": 1, "StartPositionTicks": 30_000_000}),
            ),
            (
                R::Queue {
                    item_ids: vec![s("a")],
                    next: false,
                },
                "/SyncPlay/Queue",
                json!({"ItemIds": ["a"], "Mode": "Queue"}),
            ),
            (
                R::Queue {
                    item_ids: vec![s("a")],
                    next: true,
                },
                "/SyncPlay/Queue",
                json!({"ItemIds": ["a"], "Mode": "QueueNext"}),
            ),
            (
                R::SetPlaylistItem { pid: s("p") },
                "/SyncPlay/SetPlaylistItem",
                pid.clone(),
            ),
            (
                R::NextItem { pid: s("p") },
                "/SyncPlay/NextItem",
                pid.clone(),
            ),
            (
                R::PreviousItem { pid: s("p") },
                "/SyncPlay/PreviousItem",
                pid,
            ),
            (R::Unpause, "/SyncPlay/Unpause", Value::Null),
            (R::Pause, "/SyncPlay/Pause", Value::Null),
            (R::Stop, "/SyncPlay/Stop", Value::Null),
            (
                R::Seek { position: pos },
                "/SyncPlay/Seek",
                json!({"PositionTicks": 30_000_000}),
            ),
            (
                R::Ready {
                    position: pos,
                    is_playing: false,
                    pid: s("p"),
                },
                "/SyncPlay/Ready",
                ready(false),
            ),
            (
                R::Buffering {
                    position: pos,
                    pid: s("p"),
                },
                "/SyncPlay/Buffering",
                ready(true),
            ),
        ];
        assert_eq!(table.len(), 15);
        for (req, path, body) in table {
            assert_eq!(request_path_and_body(&req, now), (path, body), "{req:?}");
        }
    }

    #[test]
    fn clock_sample_matches_ntp_formula() {
        // Server clock 1000 ahead; 40 ms each way, 10 ms server hold.
        assert_eq!(clock_sample(0, 1040, 1050, 90), (1000, 80));
        assert_eq!(clock_sample(100, 100, 100, 100), (0, 0));
    }

    #[test]
    fn parse_groups_tolerates_bad_entries() {
        let body = json!([
            {"GroupId": "g1", "GroupName": "One", "State": "Idle", "Participants": ["a", "b"]},
            {"GroupName": "no id", "Participants": []},
            "junk",
            {"GroupId": "g3", "GroupName": "Three", "Participants": []}
        ]);
        assert_eq!(
            parse_groups(&body),
            vec![
                GroupSummary {
                    id: "g1".into(),
                    name: "One".into(),
                    members: 2,
                    state: "Idle".into()
                },
                GroupSummary {
                    id: "g3".into(),
                    name: "Three".into(),
                    members: 0,
                    state: String::new()
                },
            ]
        );
        assert!(parse_groups(&json!({"a": 1})).is_empty());
    }
}
