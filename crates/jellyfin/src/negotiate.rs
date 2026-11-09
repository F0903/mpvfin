//! Capabilities and direct-play negotiation bodies, and `PlaybackInfo` parsing.

use serde_json::{Value, json};

use crate::client::{Client, HttpError};
use crate::identity::pct;
use crate::url::BaseUrl;

/// Jellyfin stream indices in track order; a track's number is its position + 1.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrackMap {
    /// Jellyfin audio stream indices.
    pub audio: Vec<i32>,
    /// Jellyfin subtitle stream indices (embedded, then external).
    pub subtitles: Vec<i32>,
}

impl TrackMap {
    fn track(list: &[i32], index: i32) -> Option<i64> {
        let position = list.iter().position(|&i| i == index)?;
        i64::try_from(position).ok().map(|p| p + 1)
    }

    fn stream(list: &[i32], track: i64) -> Option<i32> {
        let position = usize::try_from(track.checked_sub(1)?).ok()?;
        list.get(position).copied()
    }

    /// The audio track number of a Jellyfin audio stream.
    #[must_use]
    pub fn audio_track(&self, index: i32) -> Option<i64> {
        Self::track(&self.audio, index)
    }

    /// The subtitle track number of a Jellyfin subtitle stream.
    #[must_use]
    pub fn subtitle_track(&self, index: i32) -> Option<i64> {
        Self::track(&self.subtitles, index)
    }

    /// The Jellyfin audio stream of an audio track number.
    #[must_use]
    pub fn audio_index(&self, track: i64) -> Option<i32> {
        Self::stream(&self.audio, track)
    }

    /// The Jellyfin subtitle stream of a subtitle track number.
    #[must_use]
    pub fn subtitle_index(&self, track: i64) -> Option<i32> {
        Self::stream(&self.subtitles, track)
    }
}

/// How Jellyfin decided to deliver the media.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayMethod {
    /// The server file can be played without conversion.
    DirectPlay,
    /// The server repackages the source without re-encoding it.
    DirectStream,
    /// The server transcodes at least one stream.
    Transcode,
}

/// A successful result from Jellyfin playback negotiation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMedia {
    /// Jellyfin item identifier.
    pub item_id: String,
    /// Credential-free stream URL.
    pub uri: String,
    /// Display title.
    pub title: String,
    /// Jellyfin playback-session identifier.
    pub play_session_id: String,
    /// Selected Jellyfin media-source identifier.
    pub media_source_id: String,
    /// Delivery method selected by Jellyfin.
    pub play_method: PlayMethod,
    /// Credential-free external subtitle URLs, after the embedded subtitles.
    pub external_subtitles: Vec<String>,
    /// Which stream each track is.
    pub tracks: TrackMap,
}

const SUBTITLE_FORMATS: [&str; 12] = [
    "srt", "subrip", "ass", "ssa", "vtt", "webvtt", "sub", "pgs", "pgssub", "dvdsub", "dvbsub",
    "ttml",
];

fn device_profile(name: &str) -> Value {
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

/// Human-readable title for an item.
#[must_use]
pub fn display_title(item: &Value) -> String {
    let s = |k: &str| item.get(k).and_then(Value::as_str);
    let n = |k: &str| item.get(k).and_then(Value::as_i64);
    let name = s("Name").unwrap_or_default();
    if let (Some(series), Some(season), Some(ep)) =
        (s("SeriesName"), n("ParentIndexNumber"), n("IndexNumber"))
    {
        return format!("{series} S{season:02}E{ep:02} - {name}");
    }
    match n("ProductionYear") {
        Some(y) => format!("{name} ({y})"),
        None => name.to_owned(),
    }
}

/// The title without the series, for where space is tight.
#[must_use]
pub fn short_title(item: &Value) -> String {
    let s = |k: &str| item.get(k).and_then(Value::as_str);
    let n = |k: &str| item.get(k).and_then(Value::as_i64);
    match (s("SeriesName"), n("ParentIndexNumber"), n("IndexNumber")) {
        (Some(_), Some(season), Some(ep)) => {
            format!("S{season:02}E{ep:02} · {}", s("Name").unwrap_or_default())
        }
        _ => display_title(item),
    }
}

/// Drops every `ApiKey`/`api_key` query parameter (case-insensitive).
fn strip_api_key(url: &str) -> String {
    let Some((path, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| {
            let k = p.split('=').next().unwrap_or_default().to_ascii_lowercase();
            k != "apikey" && k != "api_key"
        })
        .collect();
    if kept.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{}", kept.join("&"))
    }
}

/// Picks a direct-play source and builds the credential-free media description.
///
/// # Errors
/// A user-facing message when the server refuses playback or the item would
/// need transcoding.
pub fn resolve_media(
    base: &BaseUrl,
    item_id: &str,
    item: &Value,
    playback_info: &Value,
) -> Result<ResolvedMedia, String> {
    if let Some(code) = playback_info.get("ErrorCode").filter(|c| !c.is_null()) {
        let code = code
            .as_str()
            .map_or_else(|| code.to_string(), str::to_owned);
        return Err(format!("Jellyfin refused playback: {code}"));
    }
    let flag = |s: &Value, k: &str| s.get(k).and_then(Value::as_bool);
    let source = playback_info
        .get("MediaSources")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter().find(|s| {
                flag(s, "SupportsDirectPlay") == Some(true)
                    && flag(s, "RequiresOpening") != Some(true)
                    && flag(s, "IsRemote") != Some(true)
            })
        })
        .ok_or("This item can't be played directly; the server would need to transcode it")?;
    let source_id = source
        .get("Id")
        .and_then(Value::as_str)
        .ok_or("Jellyfin returned a media source without an id")?;
    let play_session_id = playback_info
        .get("PlaySessionId")
        .and_then(Value::as_str)
        .ok_or("Jellyfin returned no play session id")?;

    let mut streams: Vec<(i32, &str, bool, Option<&str>)> = source
        .get("MediaStreams")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let index = i32::try_from(s.get("Index")?.as_i64()?).ok()?;
            let ty = s.get("Type")?.as_str()?;
            let external = flag(s, "IsExternal") == Some(true);
            Some((
                index,
                ty,
                external,
                s.get("DeliveryUrl").and_then(Value::as_str),
            ))
        })
        .collect();
    streams.sort_by_key(|s| s.0);

    let audio = streams
        .iter()
        .filter(|s| s.1 == "Audio")
        .map(|s| s.0)
        .collect();
    let subs = || streams.iter().filter(|s| s.1 == "Subtitle");
    let mut subtitles: Vec<i32> = subs().filter(|s| !s.2).map(|s| s.0).collect();
    let external: Vec<_> = subs().filter(|s| s.2 && s.3.is_some()).collect();
    subtitles.extend(external.iter().map(|s| s.0));
    let external_subtitles = external
        .iter()
        .filter_map(|s| s.3)
        .map(|u| base.join(&strip_api_key(u)))
        .collect();

    Ok(ResolvedMedia {
        item_id: item_id.to_owned(),
        uri: base.join(&format!(
            "/Videos/{item_id}/stream?static=true&mediaSourceId={source_id}"
        )),
        title: display_title(item),
        play_session_id: play_session_id.to_owned(),
        media_source_id: source_id.to_owned(),
        play_method: PlayMethod::DirectPlay,
        external_subtitles,
        tracks: TrackMap { audio, subtitles },
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

    /// Negotiates playback of an item: the media, with stream URLs built on
    /// `media_base`, and the item's `RunTimeTicks`.
    ///
    /// # Errors
    /// A request failed, or Jellyfin offers no way to play it directly.
    pub fn resolve(
        &self,
        user_id: &str,
        item_id: &str,
        media_base: &BaseUrl,
    ) -> Result<(ResolvedMedia, Option<i64>), String> {
        let failed = |e: HttpError| format!("Jellyfin request failed: {e}");
        let id = pct(item_id);
        let item = self
            .get(&format!("/Items/{id}?userId={}", pct(user_id)))
            .map_err(failed)?;
        let info = self
            .post(
                &format!("/Items/{id}/PlaybackInfo"),
                &playback_info_body(user_id, self.app.name),
            )
            .map_err(failed)?;
        let runtime = item.get("RunTimeTicks").and_then(Value::as_i64);
        resolve_media(media_base, item_id, &item, &info).map(|m| (m, runtime))
    }

    /// An item's title for where space is tight; see [`short_title`].
    ///
    /// # Errors
    /// The request failed.
    pub fn short_title(&self, user_id: &str, item_id: &str) -> Result<String, HttpError> {
        let item = self.get(&format!("/Items/{}?userId={}", pct(item_id), pct(user_id)))?;
        Ok(short_title(&item))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BaseUrl {
        BaseUrl::parse("http://h:8096").unwrap()
    }

    fn resolve(info: &Value) -> Result<ResolvedMedia, String> {
        resolve_media(&base(), "item1", &json!({"Name": "N"}), info)
    }

    fn info(source: &Value) -> Value {
        json!({"PlaySessionId": "ps", "MediaSources": [source]})
    }

    fn source(streams: &Value) -> Value {
        json!({"Id": "src", "SupportsDirectPlay": true, "MediaStreams": streams})
    }

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

    #[test]
    fn resolves_direct_play_source_to_static_stream_url() {
        let m = resolve(&info(&source(&json!([])))).unwrap();
        assert_eq!(
            m.uri,
            "http://h:8096/Videos/item1/stream?static=true&mediaSourceId=src"
        );
        assert_eq!(
            (m.media_source_id.as_str(), m.play_session_id.as_str()),
            ("src", "ps")
        );
        assert_eq!(m.play_method, PlayMethod::DirectPlay);
        assert!(!m.uri.to_lowercase().contains("api"));
    }

    #[test]
    fn track_map_orders_embedded_then_external() {
        let s = source(&json!([
            {"Index": 5, "Type": "Subtitle", "IsExternal": true, "DeliveryUrl": "/s/5.srt"},
            {"Index": 3, "Type": "Subtitle"},
            {"Index": 2, "Type": "Audio"},
            {"Index": 0, "Type": "Video"},
            {"Index": 1, "Type": "Audio"},
            {"Index": 4, "Type": "Subtitle", "IsExternal": false}
        ]));
        let m = resolve(&info(&s)).unwrap();
        assert_eq!(m.tracks.audio, vec![1, 2]);
        assert_eq!(m.tracks.subtitles, vec![3, 4, 5]);
        assert_eq!(m.external_subtitles, vec!["http://h:8096/s/5.srt"]);
    }

    #[test]
    fn delivery_url_on_a_prefixed_base_keeps_exactly_one_prefix() {
        let s = source(&json!([
            {"Index": 5, "Type": "Subtitle", "IsExternal": true, "DeliveryUrl": "/Videos/x/Stream.srt"}
        ]));
        let prefixed = BaseUrl::parse("https://h/jf/").unwrap();
        let m = resolve_media(&prefixed, "item1", &json!({"Name": "N"}), &info(&s)).unwrap();
        assert_eq!(
            m.external_subtitles,
            vec!["https://h/jf/Videos/x/Stream.srt"]
        );
    }

    #[test]
    fn delivery_url_credentials_are_stripped() {
        for key in ["ApiKey", "api_key", "APIKEY"] {
            let url = format!("/Videos/x/y/Subtitles/5/0/Stream.srt?{key}=SECRET&foo=1");
            let s = source(
                &json!([{"Index": 5, "Type": "Subtitle", "IsExternal": true, "DeliveryUrl": url}]),
            );
            let m = resolve(&info(&s)).unwrap();
            assert_eq!(
                m.external_subtitles,
                vec!["http://h:8096/Videos/x/y/Subtitles/5/0/Stream.srt?foo=1"]
            );
            assert!(!m.external_subtitles[0].contains("SECRET"));
        }
    }

    #[test]
    fn rejects_sources_needing_transcode_opening_or_remote() {
        for (k, v) in [
            ("SupportsDirectPlay", false),
            ("RequiresOpening", true),
            ("IsRemote", true),
        ] {
            let mut s = source(&json!([]));
            s[k] = json!(v);
            assert!(resolve(&info(&s)).unwrap_err().contains("transcode"), "{k}");
        }
        let mut i = info(&source(&json!([])));
        i["ErrorCode"] = json!("NotAllowed");
        assert_eq!(
            resolve(&i).unwrap_err(),
            "Jellyfin refused playback: NotAllowed"
        );
    }

    #[test]
    fn title_formats_episodes_movies_and_fallback() {
        let ep = json!({"Name": "Pilot", "SeriesName": "Test Show", "ParentIndexNumber": 1, "IndexNumber": 2});
        assert_eq!(display_title(&ep), "Test Show S01E02 - Pilot");
        assert_eq!(
            display_title(&json!({"Name": "X", "ProductionYear": 2020})),
            "X (2020)"
        );
        assert_eq!(display_title(&json!({"Name": "Name"})), "Name");
    }

    #[test]
    fn short_title_drops_the_series_from_episodes() {
        let ep = json!({"Name": "Pilot", "SeriesName": "Test Show", "ParentIndexNumber": 1, "IndexNumber": 2});
        assert_eq!(short_title(&ep), "S01E02 · Pilot");
        assert_eq!(
            short_title(&json!({"Name": "X", "ProductionYear": 2020})),
            "X (2020)"
        );
        assert_eq!(short_title(&json!({})), "");
    }

    #[test]
    fn track_map_round_trips_embedded_and_external_indices() {
        let t = TrackMap {
            audio: vec![1, 2],
            subtitles: vec![3, 4, 7],
        };
        assert_eq!(t.audio_track(2), Some(2));
        assert_eq!(t.subtitle_track(7), Some(3));
        assert_eq!(t.subtitle_index(1), Some(3));
        assert_eq!(t.audio_index(2), Some(2));
        assert_eq!(t.audio_track(9), None);
        assert_eq!(t.audio_index(0), None);
    }
}
