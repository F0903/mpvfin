//! Pure translation between the core's types and mpv commands.

use std::ffi::c_int;
use std::time::Duration;

use mpvfin_core::{EndReason, QueueMode, QueueRequest, ResolvedMedia, StreamSelection};

use crate::ffi::{MPV_END_FILE_REASON_EOF, MPV_END_FILE_REASON_ERROR};

/// URL scheme of the playlist entries mpvfin queues.
pub const ENTRY_SCHEME: &str = "mpvfin://";

/// Kind of track a Jellyfin stream index refers to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackKind {
    Audio,
    Subtitle,
}

/// Playlist entry URL for a Jellyfin item.
#[must_use]
pub fn entry_uri(item_id: &str) -> String {
    format!("{ENTRY_SCHEME}{item_id}")
}

/// Jellyfin item ID of an mpvfin playlist entry; `None` for any other path.
#[must_use]
pub fn item_id_from_entry(uri: &str) -> Option<&str> {
    uri.strip_prefix(ENTRY_SCHEME).filter(|id| !id.is_empty())
}

/// `loadfile` commands that queue the request's items.
#[must_use]
pub fn queue_commands(req: &QueueRequest) -> Vec<Vec<String>> {
    const NO_RESUME: &str = "resume-playback=no";
    let load = |id: &str, flag: &str, index: String, options: String| {
        vec![
            "loadfile".to_owned(),
            entry_uri(id),
            flag.to_owned(),
            index,
            options,
        ]
    };
    let ids = &req.item_ids;
    match req.mode {
        QueueMode::Replace => {
            let Some(last) = ids.len().checked_sub(1) else {
                return Vec::new();
            };
            let start = req.start_index.min(last);
            let options = if req.start_position.is_zero() {
                NO_RESUME.to_owned()
            } else {
                format!("start={},{NO_RESUME}", format_seconds(req.start_position))
            };
            let mut commands = vec![load(&ids[start], "replace", "-1".into(), options)];
            commands.extend(
                ids[start + 1..]
                    .iter()
                    .map(|id| load(id, "append", "-1".into(), NO_RESUME.into())),
            );
            commands.extend(
                ids[..start]
                    .iter()
                    .enumerate()
                    .map(|(i, id)| load(id, "insert-at", i.to_string(), NO_RESUME.into())),
            );
            commands
        }
        QueueMode::Next => ids
            .iter()
            .rev()
            .map(|id| load(id, "insert-next", "-1".into(), NO_RESUME.into()))
            .collect(),
        QueueMode::Last => ids
            .iter()
            .map(|id| load(id, "append", "-1".into(), NO_RESUME.into()))
            .collect(),
    }
}

/// Properties the `on_load` hook sets before continuing an mpvfin entry.
#[must_use]
pub fn hook_properties(media: &ResolvedMedia) -> Vec<(&'static str, String)> {
    vec![
        ("stream-open-filename", media.uri.clone()),
        (
            "file-local-options/http-header-fields",
            media.auth_header.expose().to_owned(),
        ),
        ("file-local-options/force-media-title", media.title.clone()),
        ("file-local-options/save-position-on-quit", "no".to_owned()),
    ]
}

/// Commands run once the file loaded: external subtitles, then the selection.
#[must_use]
pub fn after_load_commands(media: &ResolvedMedia, selection: &StreamSelection) -> Vec<Vec<String>> {
    let subtitles = media
        .external_subtitles
        .iter()
        .map(|url| vec!["sub-add".to_owned(), url.clone(), "auto".to_owned()]);
    let audio = selection
        .audio
        .and_then(|index| track_command(media, TrackKind::Audio, index));
    let subtitle = selection
        .subtitle
        .and_then(|index| track_command(media, TrackKind::Subtitle, index));
    subtitles.chain(audio).chain(subtitle).collect()
}

/// Selects the mpv track for a Jellyfin stream index; subtitle `-1` is off.
/// `None` if the index does not map to a track.
#[must_use]
pub fn track_command(
    media: &ResolvedMedia,
    kind: TrackKind,
    jellyfin_index: i32,
) -> Option<Vec<String>> {
    let (property, value) = match kind {
        TrackKind::Audio => (
            "file-local-options/aid",
            media.tracks.mpv_audio_id(jellyfin_index)?.to_string(),
        ),
        TrackKind::Subtitle if jellyfin_index == -1 => ("file-local-options/sid", "no".to_owned()),
        TrackKind::Subtitle => (
            "file-local-options/sid",
            media.tracks.mpv_subtitle_id(jellyfin_index)?.to_string(),
        ),
    };
    Some(vec!["set".to_owned(), property.to_owned(), value])
}

/// Maps an `mpv_end_file_reason` to the core's end reason.
#[must_use]
pub fn end_reason(reason: c_int, error: Option<String>) -> EndReason {
    match reason {
        MPV_END_FILE_REASON_EOF => EndReason::Eof,
        MPV_END_FILE_REASON_ERROR => {
            EndReason::Failed(error.unwrap_or_else(|| "playback failed".to_owned()))
        }
        // STOP, QUIT, REDIRECT, and reasons newer than this client.h.
        _ => EndReason::Stopped,
    }
}

/// Seconds with millisecond precision, as mpv's `start` option takes them.
#[must_use]
pub fn format_seconds(d: Duration) -> String {
    format!("{}.{:03}", d.as_secs(), d.subsec_millis())
}

/// Escapes `$` so mpv's property expansion prints `text` literally.
#[must_use]
pub fn escape_osd(text: &str) -> String {
    text.replace('$', "$$")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::{
        MPV_END_FILE_REASON_QUIT, MPV_END_FILE_REASON_REDIRECT, MPV_END_FILE_REASON_STOP,
    };
    use mpvfin_core::{PlayMethod, Secret, TrackMap};

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn request(mode: QueueMode, ids: &[&str], start_index: usize, secs: u64) -> QueueRequest {
        QueueRequest {
            item_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            mode,
            start_index,
            start_position: Duration::from_secs(secs),
            selection: StreamSelection::default(),
        }
    }

    fn media() -> ResolvedMedia {
        ResolvedMedia {
            item_id: "abc".into(),
            uri: "https://jf.invalid/Videos/abc/stream?static=true".into(),
            title: "Test Show - S01E01".into(),
            play_session_id: "ps".into(),
            media_source_id: "ms".into(),
            play_method: PlayMethod::DirectPlay,
            auth_header: Secret::new("Authorization: MediaBrowser Token=\"tok\"".into()),
            external_subtitles: vec!["https://jf.invalid/abc/Subtitles/0/Stream.srt".into()],
            tracks: TrackMap {
                audio: vec![1, 2],
                subtitles: vec![3, 4, 5],
            },
        }
    }

    #[test]
    fn replace_queue_loads_start_item_then_rest_in_order() {
        let req = request(QueueMode::Replace, &["a", "b", "c", "d"], 2, 30);
        assert_eq!(
            queue_commands(&req),
            vec![
                cmd(&[
                    "loadfile",
                    "mpvfin://c",
                    "replace",
                    "-1",
                    "start=30.000,resume-playback=no"
                ]),
                cmd(&[
                    "loadfile",
                    "mpvfin://d",
                    "append",
                    "-1",
                    "resume-playback=no"
                ]),
                cmd(&[
                    "loadfile",
                    "mpvfin://a",
                    "insert-at",
                    "0",
                    "resume-playback=no"
                ]),
                cmd(&[
                    "loadfile",
                    "mpvfin://b",
                    "insert-at",
                    "1",
                    "resume-playback=no"
                ]),
            ]
        );
    }

    #[test]
    fn replace_from_zero_has_no_start_option() {
        let req = request(QueueMode::Replace, &["a", "b"], 0, 0);
        assert_eq!(
            queue_commands(&req),
            vec![
                cmd(&[
                    "loadfile",
                    "mpvfin://a",
                    "replace",
                    "-1",
                    "resume-playback=no"
                ]),
                cmd(&[
                    "loadfile",
                    "mpvfin://b",
                    "append",
                    "-1",
                    "resume-playback=no"
                ]),
            ]
        );
    }

    #[test]
    fn play_next_inserts_in_reverse_and_play_last_appends() {
        assert_eq!(
            queue_commands(&request(QueueMode::Next, &["a", "b"], 0, 0)),
            vec![
                cmd(&[
                    "loadfile",
                    "mpvfin://b",
                    "insert-next",
                    "-1",
                    "resume-playback=no"
                ]),
                cmd(&[
                    "loadfile",
                    "mpvfin://a",
                    "insert-next",
                    "-1",
                    "resume-playback=no"
                ]),
            ]
        );
        assert_eq!(
            queue_commands(&request(QueueMode::Last, &["a", "b"], 0, 0)),
            vec![
                cmd(&[
                    "loadfile",
                    "mpvfin://a",
                    "append",
                    "-1",
                    "resume-playback=no"
                ]),
                cmd(&[
                    "loadfile",
                    "mpvfin://b",
                    "append",
                    "-1",
                    "resume-playback=no"
                ]),
            ]
        );
    }

    #[test]
    fn hook_sets_stream_header_title_and_disables_watch_later() {
        assert_eq!(
            hook_properties(&media()),
            vec![
                (
                    "stream-open-filename",
                    "https://jf.invalid/Videos/abc/stream?static=true".to_owned()
                ),
                (
                    "file-local-options/http-header-fields",
                    "Authorization: MediaBrowser Token=\"tok\"".to_owned()
                ),
                (
                    "file-local-options/force-media-title",
                    "Test Show - S01E01".to_owned()
                ),
                ("file-local-options/save-position-on-quit", "no".to_owned()),
            ]
        );
    }

    #[test]
    fn after_load_adds_external_subs_then_applies_selection() {
        let m = media();
        let sub_add = cmd(&[
            "sub-add",
            "https://jf.invalid/abc/Subtitles/0/Stream.srt",
            "auto",
        ]);
        let selection = StreamSelection {
            audio: Some(2),
            subtitle: Some(5),
        };
        assert_eq!(
            after_load_commands(&m, &selection),
            vec![
                sub_add.clone(),
                cmd(&["set", "file-local-options/aid", "2"]),
                cmd(&["set", "file-local-options/sid", "3"]),
            ]
        );
        let off = StreamSelection {
            audio: None,
            subtitle: Some(-1),
        };
        assert_eq!(
            after_load_commands(&m, &off),
            vec![sub_add, cmd(&["set", "file-local-options/sid", "no"])]
        );
    }

    #[test]
    fn entries_are_recognised_and_foreign_paths_are_not() {
        assert_eq!(entry_uri("abc"), "mpvfin://abc");
        assert_eq!(item_id_from_entry("mpvfin://abc"), Some("abc"));
        assert_eq!(item_id_from_entry(r"C:\v.mkv"), None);
        assert_eq!(item_id_from_entry("https://x/y"), None);
    }

    #[test]
    fn end_file_reasons_map_to_end_reasons() {
        assert_eq!(end_reason(MPV_END_FILE_REASON_EOF, None), EndReason::Eof);
        for reason in [
            MPV_END_FILE_REASON_STOP,
            MPV_END_FILE_REASON_QUIT,
            MPV_END_FILE_REASON_REDIRECT,
        ] {
            assert_eq!(end_reason(reason, None), EndReason::Stopped);
        }
        assert_eq!(
            end_reason(MPV_END_FILE_REASON_ERROR, Some("loading failed".into())),
            EndReason::Failed("loading failed".into())
        );
    }

    #[test]
    fn osd_text_escapes_property_expansion() {
        assert_eq!(escape_osd("a ${x} $"), "a $${x} $$");
    }
}
