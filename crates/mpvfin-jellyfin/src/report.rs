//! JSON bodies for Jellyfin playback reports.

use mpvfin_core::{PlayMethod, PlaybackSnapshot};
use serde_json::{Value, json};

use crate::wire::duration_to_ticks;

fn live_body(s: &PlaybackSnapshot) -> Value {
    let method = match s.play_method {
        PlayMethod::DirectPlay => "DirectPlay",
        PlayMethod::DirectStream => "DirectStream",
        PlayMethod::Transcode => "Transcode",
    };
    let mut body = json!({
        "ItemId": s.item_id,
        "MediaSourceId": s.media_source_id,
        "PlaySessionId": s.play_session_id,
        "PlayMethod": method,
        "PositionTicks": duration_to_ticks(s.position),
        "IsPaused": s.paused,
        "IsMuted": s.muted,
        "VolumeLevel": s.volume_percent,
        "SubtitleStreamIndex": s.subtitle_stream_index.unwrap_or(-1),
        "CanSeek": true,
    });
    if let (Some(a), Some(map)) = (s.audio_stream_index, body.as_object_mut()) {
        map.insert("AudioStreamIndex".into(), json!(a));
    }
    body
}

/// Body for `POST /Sessions/Playing`.
#[must_use]
pub fn started_body(s: &PlaybackSnapshot) -> Value {
    live_body(s)
}

/// Body for `POST /Sessions/Playing/Progress`.
#[must_use]
pub fn progress_body(s: &PlaybackSnapshot) -> Value {
    live_body(s)
}

/// Body for `POST /Sessions/Playing/Stopped`.
#[must_use]
pub fn stopped_body(s: &PlaybackSnapshot, failed: bool) -> Value {
    json!({
        "ItemId": s.item_id,
        "MediaSourceId": s.media_source_id,
        "PlaySessionId": s.play_session_id,
        "PositionTicks": duration_to_ticks(s.position),
        "Failed": failed,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mpvfin_core::Generation;

    use super::*;

    fn snap() -> PlaybackSnapshot {
        PlaybackSnapshot {
            generation: Generation::new(1).unwrap(),
            item_id: "item".into(),
            play_session_id: "ps".into(),
            media_source_id: "ms".into(),
            play_method: PlayMethod::DirectPlay,
            position: Duration::from_secs(30),
            paused: true,
            volume_percent: 40,
            muted: false,
            audio_stream_index: Some(2),
            subtitle_stream_index: None,
        }
    }

    #[test]
    fn start_report_body_matches_snapshot() {
        let expected = json!({
            "ItemId": "item", "MediaSourceId": "ms", "PlaySessionId": "ps",
            "PlayMethod": "DirectPlay", "PositionTicks": 300_000_000,
            "IsPaused": true, "IsMuted": false, "VolumeLevel": 40,
            "AudioStreamIndex": 2, "SubtitleStreamIndex": -1, "CanSeek": true,
        });
        assert_eq!(started_body(&snap()), expected);
        assert_eq!(progress_body(&snap()), expected);
    }

    #[test]
    fn absent_audio_index_is_omitted() {
        let mut s = snap();
        s.audio_stream_index = None;
        s.play_method = PlayMethod::Transcode;
        let b = progress_body(&s);
        assert!(b.get("AudioStreamIndex").is_none());
        assert_eq!(b["PlayMethod"], "Transcode");
    }

    #[test]
    fn stopped_report_marks_failure() {
        assert_eq!(
            stopped_body(&snap(), true),
            json!({"ItemId":"item","MediaSourceId":"ms","PlaySessionId":"ps",
                   "PositionTicks":300_000_000,"Failed":true})
        );
        assert_eq!(stopped_body(&snap(), false)["Failed"], false);
    }
}
