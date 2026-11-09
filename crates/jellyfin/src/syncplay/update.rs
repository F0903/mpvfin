//! What the server tells a group: queue updates, commands and membership.

use std::time::Duration;

use serde_json::Value;

use crate::time::parse_utc_ms;
use crate::time::ticks_to_duration;

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

pub(super) fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

pub(super) fn ticks(v: &Value, key: &str) -> Duration {
    ticks_to_duration(v.get(key).and_then(Value::as_i64).unwrap_or(0))
}

pub(super) fn parse_command(data: &Value) -> Option<GroupUpdate> {
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

pub(super) fn parse_queue(data: &Value) -> Option<GroupUpdate> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn group(kind: &str, data: &Value) -> GroupUpdate {
        parse_group_message(
            "SyncPlayGroupUpdate",
            &json!({"GroupId": "g1", "Type": kind, "Data": data}),
        )
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
    fn queue_of(u: GroupUpdate) -> GroupQueue {
        match u {
            GroupUpdate::Queue(q) => q,
            other => panic!("not a queue: {other:?}"),
        }
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
}
