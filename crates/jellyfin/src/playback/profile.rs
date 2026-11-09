//! What this client tells the server it can play and do.

use serde_json::{Value, json};

use crate::client::{Client, HttpError};

pub(super) const SUBTITLE_FORMATS: [&str; 12] = [
    "srt", "subrip", "ass", "ssa", "vtt", "webvtt", "sub", "pgs", "pgssub", "dvdsub", "dvbsub",
    "ttml",
];

pub(super) fn device_profile(name: &str) -> Value {
    let subs: Vec<Value> = SUBTITLE_FORMATS
        .iter()
        .flat_map(|f| ["Embed", "External"].map(|m| json!({"Format": f, "Method": m})))
        .collect();
    json!({
        "Name": name,
        "MaxStreamingBitrate": 1_000_000_000,
        "MaxStaticBitrate": 1_000_000_000,
        "DirectPlayProfiles": [{"Type": "Video"}],
        "TranscodingProfiles": [],
        "SubtitleProfiles": subs,
    })
}

/// Body for `POST /Sessions/Capabilities/Full`.
#[must_use]
pub fn capabilities_body(client: &str) -> Value {
    json!({
        "PlayableMediaTypes": ["Video"],
        "SupportedCommands": ["SetVolume", "Mute", "Unmute", "ToggleMute",
            "SetAudioStreamIndex", "SetSubtitleStreamIndex", "DisplayMessage"],
        "SupportsMediaControl": true,
        "SupportsPersistentIdentifier": true,
        "DeviceProfile": device_profile(client),
    })
}

/// Body for `POST /Items/{id}/PlaybackInfo` (direct play only).
#[must_use]
pub fn playback_info_body(user_id: &str, client: &str) -> Value {
    json!({
        "UserId": user_id,
        "DeviceProfile": device_profile(client),
        "IsPlayback": true,
        "AutoOpenLiveStream": false,
        "EnableDirectPlay": true,
        "EnableDirectStream": false,
        "EnableTranscoding": false,
        "MaxStreamingBitrate": 1_000_000_000,
    })
}

impl Client {
    /// Tells the server what this client can be asked to do.
    ///
    /// # Errors
    /// The request failed; `HttpError::Unauthorized` means the login is gone.
    pub fn report_capabilities(&self) -> Result<(), HttpError> {
        self.post(
            "/Sessions/Capabilities/Full",
            &capabilities_body(self.app.name),
        )
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_advertise_only_implemented_commands() {
        let c = capabilities_body("mpvfin");
        assert_eq!(
            c["SupportedCommands"],
            json!([
                "SetVolume",
                "Mute",
                "Unmute",
                "ToggleMute",
                "SetAudioStreamIndex",
                "SetSubtitleStreamIndex",
                "DisplayMessage"
            ])
        );
        assert_eq!(c["PlayableMediaTypes"], json!(["Video"]));
        assert_eq!(
            c["DeviceProfile"]["SubtitleProfiles"]
                .as_array()
                .unwrap()
                .len(),
            24
        );
    }
    #[test]
    fn playback_info_request_disables_transcoding() {
        let b = playback_info_body("u1", "mpvfin");
        assert_eq!(b["UserId"], "u1");
        assert_eq!(b["EnableDirectPlay"], true);
        assert_eq!(b["EnableDirectStream"], false);
        assert_eq!(b["EnableTranscoding"], false);
        assert_eq!(b["AutoOpenLiveStream"], false);
        assert_eq!(b["IsPlayback"], true);
        assert_eq!(b["DeviceProfile"]["TranscodingProfiles"], json!([]));
    }
}
