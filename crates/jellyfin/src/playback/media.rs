//! Negotiating how to play an item, and the media that comes of it.

use serde_json::Value;

use crate::client::{Client, HttpError};
use crate::identity::pct;
use crate::playback::items::display_title;
use crate::playback::profile::playback_info_body;
use crate::remote::StreamSelection;
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
    pub(super) fn track(list: &[i32], index: i32) -> Option<i64> {
        let position = list.iter().position(|&i| i == index)?;
        i64::try_from(position).ok().map(|p| p + 1)
    }

    pub(super) fn stream(list: &[i32], track: i64) -> Option<i32> {
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
    /// The server's picks from the user's language settings and remembered
    /// choices, for the streams a controller leaves out.
    pub defaults: StreamSelection,
}

/// Drops every `ApiKey`/`api_key` query parameter (case-insensitive).
pub(super) fn strip_api_key(url: &str) -> String {
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
    let default = |key: &str| {
        let index = source.get(key).and_then(Value::as_i64)?;
        i32::try_from(index).ok()
    };

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
        defaults: StreamSelection {
            audio: default("DefaultAudioStreamIndex"),
            subtitle: default("DefaultSubtitleStreamIndex"),
        },
    })
}

impl Client {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn keeps_the_servers_default_streams() {
        let mut s = source(&json!([]));
        s["DefaultAudioStreamIndex"] = json!(1);
        s["DefaultSubtitleStreamIndex"] = json!(-1);
        assert_eq!(
            resolve(&info(&s)).unwrap().defaults,
            StreamSelection {
                audio: Some(1),
                subtitle: Some(-1),
            }
        );
        let none = resolve(&info(&source(&json!([])))).unwrap();
        assert_eq!(none.defaults, StreamSelection::default());
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
