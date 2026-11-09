//! JSON bodies for Jellyfin playback reports.

use std::time::Duration;

use serde_json::{Value, json};

use crate::client::{Client, HttpError};
use crate::identity::pct;
use crate::playback::media::PlayMethod;
use crate::time::duration_to_ticks;

/// What a playback report says about the current item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackProgress {
    /// Jellyfin item identifier.
    pub item_id: String,
    /// Jellyfin playback-session identifier.
    pub play_session_id: String,
    /// Selected Jellyfin media-source identifier.
    pub media_source_id: String,
    /// Delivery method selected by Jellyfin.
    pub play_method: PlayMethod,
    /// Position in the item.
    pub position: Duration,
    /// Whether playback is paused.
    pub paused: bool,
    /// Volume, from 0 through 100.
    pub volume_percent: u8,
    /// Whether playback is muted.
    pub muted: bool,
    /// Jellyfin audio stream index.
    pub audio_stream_index: Option<i32>,
    /// Jellyfin subtitle stream index; `None` is off.
    pub subtitle_stream_index: Option<i32>,
}

fn live_body(s: &PlaybackProgress) -> Value {
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
pub fn started_body(s: &PlaybackProgress) -> Value {
    live_body(s)
}

/// Body for `POST /Sessions/Playing/Progress`.
#[must_use]
pub fn progress_body(s: &PlaybackProgress) -> Value {
    live_body(s)
}

/// Body for `POST /Sessions/Playing/Stopped`.
#[must_use]
pub fn stopped_body(s: &PlaybackProgress, failed: bool) -> Value {
    json!({
        "ItemId": s.item_id,
        "MediaSourceId": s.media_source_id,
        "PlaySessionId": s.play_session_id,
        "PositionTicks": duration_to_ticks(s.position),
        "Failed": failed,
    })
}

impl Client {
    /// Reports that playback started.
    ///
    /// # Errors
    /// The request failed.
    pub fn report_started(&self, progress: &PlaybackProgress) -> Result<(), HttpError> {
        self.post("/Sessions/Playing", &started_body(progress))
            .map(drop)
    }

    /// Reports playback progress.
    ///
    /// # Errors
    /// The request failed.
    pub fn report_progress(&self, progress: &PlaybackProgress) -> Result<(), HttpError> {
        self.post("/Sessions/Playing/Progress", &progress_body(progress))
            .map(drop)
    }

    /// Reports that playback stopped, or failed.
    ///
    /// # Errors
    /// The request failed.
    pub fn report_stopped(
        &self,
        progress: &PlaybackProgress,
        failed: bool,
    ) -> Result<(), HttpError> {
        self.post("/Sessions/Playing/Stopped", &stopped_body(progress, failed))
            .map(drop)
    }

    /// Marks an item played for a user.
    ///
    /// # Errors
    /// The request failed.
    pub fn mark_played(&self, user_id: &str, item_id: &str) -> Result<(), HttpError> {
        let path = format!("/UserPlayedItems/{}?userId={}", pct(item_id), pct(user_id));
        self.post(&path, &Value::Null).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> PlaybackProgress {
        PlaybackProgress {
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
