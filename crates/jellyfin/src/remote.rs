//! Inbound Jellyfin WebSocket messages.

use std::time::Duration;

use serde_json::Value;

use crate::syncplay::GroupUpdate;
use crate::syncplay::parse_group_message;
use crate::time::ticks_to_duration;

/// Where queued items go relative to the player's queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueMode {
    /// Replace the playlist and start playing.
    Replace,
    /// Insert after the current entry.
    Next,
    /// Append to the end.
    Last,
}

/// Jellyfin stream indices chosen by a controller.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StreamSelection {
    /// Jellyfin audio stream index.
    pub audio: Option<i32>,
    /// Jellyfin subtitle stream index; -1 means off.
    pub subtitle: Option<i32>,
}

/// A remote request to queue Jellyfin items.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueRequest {
    /// Jellyfin item identifiers, in order.
    pub item_ids: Vec<String>,
    /// Where to put the items.
    pub mode: QueueMode,
    /// Index of the item to start; `Replace` only, clamped to the last item.
    pub start_index: usize,
    /// Starting position of the start item; `Replace` only.
    pub start_position: Duration,
    /// Stream selection for the start item; `Replace` only.
    pub selection: StreamSelection,
}

/// A remote-control command from a Jellyfin client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteCommand {
    /// Queue items.
    Play(QueueRequest),
    /// Pause.
    Pause,
    /// Resume.
    Resume,
    /// Toggle pause.
    TogglePause,
    /// Stop.
    Stop,
    /// Go to the next playlist entry.
    Next,
    /// Go to the previous playlist entry.
    Previous,
    /// Seek to an absolute position.
    Seek(Duration),
    /// Set the volume, from 0 through 100.
    SetVolume(u8),
    /// Set the mute state.
    SetMuted(bool),
    /// Toggle mute.
    ToggleMute,
    /// Select a Jellyfin audio stream.
    SetAudioStream(i32),
    /// Select a Jellyfin subtitle stream (-1 is off).
    SetSubtitleStream(i32),
}

/// What an inbound WebSocket text frame means.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Inbound {
    /// A remote-control command.
    Command(RemoteCommand),
    /// An on-screen message request.
    DisplayMessage {
        /// Header and text, newline separated.
        text: String,
        /// How long to show it.
        duration: Duration,
    },
    /// The server's keep-alive interval.
    ForceKeepAlive(Duration),
    /// A `SyncPlay` command or group update.
    Group(GroupUpdate),
    /// Anything unknown, malformed or unsupported.
    Ignored,
}

fn int(v: &Value, key: &str) -> Option<i64> {
    v.get(key)?.as_i64()
}

fn index(v: &Value, key: &str) -> Option<i32> {
    i32::try_from(int(v, key)?).ok()
}

/// Reads a `GeneralCommand` argument; Jellyfin sends them as strings.
fn arg(args: &Value, key: &str) -> Option<i64> {
    match args.get(key)? {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

fn parse_play(data: &Value) -> Option<RemoteCommand> {
    let item_ids: Vec<String> = data
        .get("ItemIds")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if item_ids.is_empty() {
        return None;
    }
    let mode = match data.get("PlayCommand")?.as_str()? {
        "PlayNow" => QueueMode::Replace,
        "PlayNext" => QueueMode::Next,
        "PlayLast" => QueueMode::Last,
        _ => return None,
    };
    let (start_index, start_position, selection) = if mode == QueueMode::Replace {
        (
            int(data, "StartIndex")
                .and_then(|i| usize::try_from(i).ok())
                .unwrap_or(0),
            ticks_to_duration(int(data, "StartPositionTicks").unwrap_or(0)),
            StreamSelection {
                audio: index(data, "AudioStreamIndex"),
                subtitle: index(data, "SubtitleStreamIndex"),
            },
        )
    } else {
        (0, Duration::ZERO, StreamSelection::default())
    };
    Some(RemoteCommand::Play(QueueRequest {
        item_ids,
        mode,
        start_index,
        start_position,
        selection,
    }))
}

fn parse_playstate(data: &Value) -> Option<RemoteCommand> {
    Some(match data.get("Command")?.as_str()? {
        "Pause" => RemoteCommand::Pause,
        "Unpause" => RemoteCommand::Resume,
        "PlayPause" => RemoteCommand::TogglePause,
        "Stop" => RemoteCommand::Stop,
        "NextTrack" => RemoteCommand::Next,
        "PreviousTrack" => RemoteCommand::Previous,
        "Seek" => RemoteCommand::Seek(ticks_to_duration(int(data, "SeekPositionTicks")?)),
        _ => return None,
    })
}

fn parse_general(data: &Value) -> Option<Inbound> {
    let args = data.get("Arguments").unwrap_or(&Value::Null);
    let event = match data.get("Name")?.as_str()? {
        "SetVolume" => {
            let v = arg(args, "Volume")?.clamp(0, 100);
            RemoteCommand::SetVolume(u8::try_from(v).ok()?)
        }
        "Mute" => RemoteCommand::SetMuted(true),
        "Unmute" => RemoteCommand::SetMuted(false),
        "ToggleMute" => RemoteCommand::ToggleMute,
        "SetAudioStreamIndex" => {
            RemoteCommand::SetAudioStream(i32::try_from(arg(args, "Index")?).ok()?)
        }
        "SetSubtitleStreamIndex" => {
            RemoteCommand::SetSubtitleStream(i32::try_from(arg(args, "Index")?).ok()?)
        }
        "DisplayMessage" => {
            let s = |k| args.get(k).and_then(Value::as_str).unwrap_or("");
            let (header, body) = (s("Header"), s("Text"));
            let text = if header.is_empty() {
                body.to_owned()
            } else {
                format!("{header}\n{body}")
            };
            let ms = arg(args, "TimeoutMs").unwrap_or(5000);
            return Some(Inbound::DisplayMessage {
                text,
                duration: Duration::from_millis(u64::try_from(ms).unwrap_or(5000)),
            });
        }
        _ => return None,
    };
    Some(Inbound::Command(event))
}

/// Parses one WebSocket text frame. Never panics; unknown input is `Ignored`.
#[must_use]
pub fn parse_inbound(text: &str) -> Inbound {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return Inbound::Ignored;
    };
    let data = msg.get("Data").unwrap_or(&Value::Null);
    let parsed = match msg.get("MessageType").and_then(Value::as_str) {
        Some("Play") => parse_play(data).map(Inbound::Command),
        Some("Playstate") => parse_playstate(data).map(Inbound::Command),
        Some("GeneralCommand") => parse_general(data),
        Some("ForceKeepAlive") => data
            .as_u64()
            .map(|s| Inbound::ForceKeepAlive(Duration::from_secs(s))),
        Some(t @ ("SyncPlayCommand" | "SyncPlayGroupUpdate")) => {
            Some(Inbound::Group(parse_group_message(t, data)))
        }
        _ => None,
    };
    parsed.unwrap_or(Inbound::Ignored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play_json(extra: &str) -> String {
        format!(r#"{{"MessageType":"Play","Data":{{{extra}}}}}"#)
    }
    fn req(mode: QueueMode, ids: &[&str], idx: usize, secs: u64, sel: StreamSelection) -> Inbound {
        Inbound::Command(RemoteCommand::Play(QueueRequest {
            item_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            mode,
            start_index: idx,
            start_position: Duration::from_secs(secs),
            selection: sel,
        }))
    }
    #[test]
    fn parses_play_now_with_start_index_ticks_and_selection() {
        let m = play_json(
            r#""ItemIds":["a","b","c"],"PlayCommand":"PlayNow","StartIndex":1,
            "StartPositionTicks":300000000,"AudioStreamIndex":2,"SubtitleStreamIndex":null"#,
        );
        let sel = StreamSelection {
            audio: Some(2),
            subtitle: None,
        };
        assert_eq!(
            parse_inbound(&m),
            req(QueueMode::Replace, &["a", "b", "c"], 1, 30, sel)
        );
    }
    #[test]
    fn routes_syncplay_messages_to_group() {
        let m = r#"{"MessageType":"SyncPlayGroupUpdate","Data":{"GroupId":"g","Type":"GroupLeft","Data":"x"}}"#;
        assert_eq!(parse_inbound(m), Inbound::Group(GroupUpdate::Left));
        let m = r#"{"MessageType":"SyncPlayCommand","Data":{"Command":"Nope"}}"#;
        assert_eq!(parse_inbound(m), Inbound::Group(GroupUpdate::Ignored));
    }
    #[test]
    fn play_next_and_play_last_map_to_queue_modes() {
        for (cmd, mode) in [("PlayNext", QueueMode::Next), ("PlayLast", QueueMode::Last)] {
            let m = play_json(&format!(
                r#""ItemIds":["x"],"PlayCommand":"{cmd}","StartIndex":null,"StartPositionTicks":null"#
            ));
            assert_eq!(
                parse_inbound(&m),
                req(mode, &["x"], 0, 0, StreamSelection::default())
            );
        }
    }
    #[test]
    fn unsupported_play_commands_and_empty_items_are_ignored() {
        assert_eq!(
            parse_inbound(&play_json(r#""ItemIds":["a"],"PlayCommand":"PlayShuffle""#)),
            Inbound::Ignored
        );
        assert_eq!(
            parse_inbound(&play_json(r#""ItemIds":[],"PlayCommand":"PlayNow""#)),
            Inbound::Ignored
        );
    }
    #[test]
    fn parses_playstate_commands() {
        use RemoteCommand as E;
        let table = [
            ("Pause", E::Pause),
            ("Unpause", E::Resume),
            ("PlayPause", E::TogglePause),
            ("Stop", E::Stop),
            ("NextTrack", E::Next),
            ("PreviousTrack", E::Previous),
        ];
        for (cmd, ev) in table {
            let m = format!(r#"{{"MessageType":"Playstate","Data":{{"Command":"{cmd}"}}}}"#);
            assert_eq!(parse_inbound(&m), Inbound::Command(ev), "{cmd}");
        }
        let seek = r#"{"MessageType":"Playstate","Data":{"Command":"Seek","SeekPositionTicks":600000000}}"#;
        assert_eq!(
            parse_inbound(seek),
            Inbound::Command(E::Seek(Duration::from_mins(1)))
        );
        let bad = r#"{"MessageType":"Playstate","Data":{"Command":"Rewind"}}"#;
        assert_eq!(parse_inbound(bad), Inbound::Ignored);
    }
    fn general(name: &str, args: &str) -> String {
        format!(
            r#"{{"MessageType":"GeneralCommand","Data":{{"Name":"{name}","Arguments":{args}}}}}"#
        )
    }
    #[test]
    fn parses_general_commands_with_string_arguments() {
        use RemoteCommand as E;
        let ok = Inbound::Command;
        let vol = |v: &str| parse_inbound(&general("SetVolume", &format!(r#"{{"Volume":"{v}"}}"#)));
        assert_eq!(vol("55"), ok(E::SetVolume(55)));
        assert_eq!(vol("500"), ok(E::SetVolume(100)));
        assert_eq!(vol("-3"), ok(E::SetVolume(0)));
        assert_eq!(vol("abc"), Inbound::Ignored);
        assert_eq!(parse_inbound(&general("SetVolume", "{}")), Inbound::Ignored);
        assert_eq!(
            parse_inbound(&general("Mute", "null")),
            ok(E::SetMuted(true))
        );
        assert_eq!(
            parse_inbound(&general("Unmute", "{}")),
            ok(E::SetMuted(false))
        );
        assert_eq!(
            parse_inbound(&general("ToggleMute", "{}")),
            ok(E::ToggleMute)
        );
        assert_eq!(
            parse_inbound(&general("SetAudioStreamIndex", r#"{"Index":"2"}"#)),
            ok(E::SetAudioStream(2))
        );
        assert_eq!(
            parse_inbound(&general("SetSubtitleStreamIndex", r#"{"Index":"-1"}"#)),
            ok(E::SetSubtitleStream(-1))
        );
        assert_eq!(
            parse_inbound(&general("SetAudioStreamIndex", r#"{"Index":"x"}"#)),
            Inbound::Ignored
        );
    }
    #[test]
    fn display_message_joins_header_and_text() {
        let with = general(
            "DisplayMessage",
            r#"{"Header":"Hi","Text":"there","TimeoutMs":"2500"}"#,
        );
        assert_eq!(
            parse_inbound(&with),
            Inbound::DisplayMessage {
                text: "Hi\nthere".into(),
                duration: Duration::from_millis(2500)
            }
        );
        let without = general("DisplayMessage", r#"{"Header":"","Text":"only"}"#);
        assert_eq!(
            parse_inbound(&without),
            Inbound::DisplayMessage {
                text: "only".into(),
                duration: Duration::from_secs(5)
            }
        );
    }
    #[test]
    fn force_keep_alive_sets_interval() {
        assert_eq!(
            parse_inbound(r#"{"MessageType":"ForceKeepAlive","Data":60}"#),
            Inbound::ForceKeepAlive(Duration::from_mins(1))
        );
    }
    #[test]
    fn unknown_messages_and_fields_are_tolerated() {
        assert_eq!(
            parse_inbound(r#"{"MessageType":"Sessions","Data":[]}"#),
            Inbound::Ignored
        );
        let m = play_json(r#""ItemIds":["a"],"PlayCommand":"PlayNow","Surprise":{"x":1}"#);
        assert_eq!(
            parse_inbound(&m),
            req(QueueMode::Replace, &["a"], 0, 0, StreamSelection::default())
        );
        for junk in ["not json", "", "null", "[]", r#"{"MessageType":5}"#] {
            assert_eq!(parse_inbound(junk), Inbound::Ignored, "{junk}");
        }
    }
}
