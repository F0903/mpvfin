//! Loading an entry: its per-file options, subtitles and track selection,
//! seeking, and how it ends.

use std::ffi::c_int;
use std::time::Duration;

use jellyfin::{ResolvedMedia, StreamSelection};

use super::mpv::ffi::{MPV_END_FILE_REASON_EOF, MPV_END_FILE_REASON_ERROR};
use crate::receiver::{EndReason, Generation};

pub(super) const NO_RESUME: &str = "resume-playback=no";

/// Kind of track a Jellyfin stream index refers to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackKind {
    Audio,
    Subtitle,
}

/// Properties the `on_load` hook sets before continuing an mpvfin entry.
///
/// `watch_later_dir` must be a path mpv cannot create: `quit-watch-later`
/// ignores `save-position-on-quit`, so only a failing write keeps it out.
#[must_use]
pub fn hook_properties(
    media: &ResolvedMedia,
    watch_later_dir: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("stream-open-filename", media.uri.clone()),
        ("file-local-options/force-media-title", media.title.clone()),
        ("file-local-options/save-position-on-quit", "no".to_owned()),
        (
            "file-local-options/watch-later-dir",
            watch_later_dir.to_owned(),
        ),
    ]
}

/// The `keep-open` of an entry of a `SyncPlay` queue.
pub const GROUP_KEEP_OPEN: &str = "always";

/// What the hook adds for an entry of a `SyncPlay` queue: it loads paused until
/// the group starts, and holds at the end until the server moves on, without
/// `keep-open-pause` so that holding doesn't look like a local pause.
#[must_use]
pub fn group_hook_properties(start: Duration) -> Vec<(&'static str, String)> {
    vec![
        ("file-local-options/pause", "yes".to_owned()),
        ("file-local-options/keep-open", GROUP_KEEP_OPEN.to_owned()),
        ("file-local-options/keep-open-pause", "no".to_owned()),
        ("file-local-options/start", format_seconds(start)),
    ]
}

/// `sub-add` commands for the external subtitles, in track order.
#[must_use]
pub fn sub_add_commands(media: &ResolvedMedia) -> Vec<Vec<String>> {
    media
        .external_subtitles
        .iter()
        .map(|url| vec!["sub-add".to_owned(), url.clone(), "auto".to_owned()])
        .collect()
}

/// Commands applying the selection; they run after the `sub-add` commands.
#[must_use]
pub fn selection_commands(media: &ResolvedMedia, selection: &StreamSelection) -> Vec<Vec<String>> {
    let audio = selection
        .audio
        .and_then(|index| track_command(media, TrackKind::Audio, index));
    let subtitle = selection
        .subtitle
        .and_then(|index| track_command(media, TrackKind::Subtitle, index));
    audio.into_iter().chain(subtitle).collect()
}

/// What a generation's subtitle setup does next.
#[derive(Debug, Eq, PartialEq)]
pub enum SubtitleStep {
    /// Issue this `sub-add` and wait for its reply.
    Add(Vec<String>),
    /// Every subtitle is in; apply the selection.
    Select(Vec<Vec<String>>),
}

/// A generation's external subtitles, added one at a time: mpv numbers tracks
/// in completion order, and `TrackMap` relies on them matching list order. The
/// selection waits for all of them because it may name one of their tracks.
pub struct SubtitleWait {
    generation: Generation,
    sub_adds: std::vec::IntoIter<Vec<String>>,
    selection: Vec<Vec<String>>,
}

impl SubtitleWait {
    /// Prepares `sub_adds`, whose replies carry `generation` as their reply id.
    #[must_use]
    pub fn new(
        generation: Generation,
        sub_adds: Vec<Vec<String>>,
        selection: Vec<Vec<String>>,
    ) -> Self {
        Self {
            generation,
            sub_adds: sub_adds.into_iter(),
            selection,
        }
    }

    /// The generation these `sub-add`s belong to.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// Whether a command reply belongs to this generation's `sub-add`s.
    #[must_use]
    pub const fn accepts(&self, reply_id: u64) -> bool {
        reply_id == self.generation.get()
    }

    /// The next thing to do; call once at the start and once per reply.
    pub fn step(&mut self) -> SubtitleStep {
        self.sub_adds.next().map_or_else(
            || SubtitleStep::Select(std::mem::take(&mut self.selection)),
            SubtitleStep::Add,
        )
    }
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
            media.tracks.audio_track(jellyfin_index)?.to_string(),
        ),
        TrackKind::Subtitle if jellyfin_index == -1 => ("file-local-options/sid", "no".to_owned()),
        TrackKind::Subtitle => (
            "file-local-options/sid",
            media.tracks.subtitle_track(jellyfin_index)?.to_string(),
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

/// Whether `eof-reached` came at the item's end, not from a stream giving out
/// early; an unknown duration counts as the end.
#[must_use]
pub fn is_true_end(position: Duration, duration: Option<Duration>) -> bool {
    duration.is_none_or(|d| d.saturating_sub(position) <= Duration::from_secs(2))
}

/// Seeks to an absolute position. Exact, because with `hr-seek=no` a plain seek
/// lands on a keyframe, possibly inside the segment being skipped.
#[must_use]
pub fn seek_command(position: Duration) -> Vec<String> {
    vec![
        "seek".to_owned(),
        format_seconds(position),
        "absolute+exact".to_owned(),
    ]
}

/// Seconds with millisecond precision, as mpv's `start` option takes them.
#[must_use]
pub fn format_seconds(d: Duration) -> String {
    format!("{}.{:03}", d.as_secs(), d.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::Generation;
    use crate::runtime::mpv::ffi::{
        MPV_END_FILE_REASON_QUIT, MPV_END_FILE_REASON_REDIRECT, MPV_END_FILE_REASON_STOP,
    };
    use jellyfin::{PlayMethod, TrackMap};

    #[test]
    fn a_true_end_is_within_two_seconds_of_the_duration() {
        let ms = Duration::from_millis;
        let length = Some(ms(100_000));
        assert!(is_true_end(ms(100_000), length));
        assert!(is_true_end(ms(98_000), length));
        assert!(!is_true_end(ms(97_999), length), "stopped early");
        assert!(is_true_end(ms(105_000), length), "past the known length");
        assert!(is_true_end(ms(3_000), None), "unknown duration");
    }
    fn media() -> ResolvedMedia {
        ResolvedMedia {
            item_id: "abc".into(),
            uri: "https://jf.invalid/Videos/abc/stream?static=true".into(),
            title: "Test Show - S01E01".into(),
            play_session_id: "ps".into(),
            media_source_id: "ms".into(),
            play_method: PlayMethod::DirectPlay,
            external_subtitles: vec!["https://jf.invalid/abc/Subtitles/0/Stream.srt".into()],
            tracks: TrackMap {
                audio: vec![1, 2],
                subtitles: vec![3, 4, 5],
            },
        }
    }
    #[test]
    fn hook_sets_stream_title_and_disables_watch_later() {
        let properties = hook_properties(&media(), "state.json/watch_later");
        assert_eq!(
            properties,
            vec![
                (
                    "stream-open-filename",
                    "https://jf.invalid/Videos/abc/stream?static=true".to_owned()
                ),
                (
                    "file-local-options/force-media-title",
                    "Test Show - S01E01".to_owned()
                ),
                ("file-local-options/save-position-on-quit", "no".to_owned()),
                (
                    "file-local-options/watch-later-dir",
                    "state.json/watch_later".to_owned()
                ),
            ]
        );
        assert!(
            properties
                .iter()
                .all(|(name, _)| !name.contains("http-header-fields"))
        );
    }
    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }
    #[test]
    fn sub_adds_are_split_from_the_selection() {
        let m = media();
        assert_eq!(
            sub_add_commands(&m),
            vec![cmd(&[
                "sub-add",
                "https://jf.invalid/abc/Subtitles/0/Stream.srt",
                "auto",
            ])]
        );
        let selection = StreamSelection {
            audio: Some(2),
            subtitle: Some(5),
        };
        assert_eq!(
            selection_commands(&m, &selection),
            vec![
                cmd(&["set", "file-local-options/aid", "2"]),
                cmd(&["set", "file-local-options/sid", "3"]),
            ]
        );
        let off = StreamSelection {
            audio: None,
            subtitle: Some(-1),
        };
        assert_eq!(
            selection_commands(&m, &off),
            vec![cmd(&["set", "file-local-options/sid", "no"])]
        );
    }
    #[test]
    fn subtitles_are_added_in_track_order_then_the_selection_runs() {
        let generation = Generation::new(7).unwrap();
        let first = cmd(&["sub-add", "http://x/a", "auto"]);
        let second = cmd(&["sub-add", "http://x/b", "auto"]);
        let selection = vec![cmd(&["set", "file-local-options/sid", "3"])];
        let mut wait = SubtitleWait::new(
            generation,
            vec![first.clone(), second.clone()],
            selection.clone(),
        );
        assert!(!wait.accepts(6), "another generation's reply");
        assert!(wait.accepts(7));
        assert_eq!(wait.step(), SubtitleStep::Add(first));
        assert_eq!(wait.step(), SubtitleStep::Add(second));
        assert_eq!(wait.step(), SubtitleStep::Select(selection));
    }
    #[test]
    fn with_no_external_subtitles_the_selection_runs_at_once() {
        let selection = vec![cmd(&["set", "file-local-options/aid", "2"])];
        let mut wait = SubtitleWait::new(Generation::new(7).unwrap(), vec![], selection.clone());
        assert_eq!(wait.step(), SubtitleStep::Select(selection));
    }
    #[test]
    fn seeks_are_exact() {
        assert_eq!(
            seek_command(Duration::from_millis(20_500)),
            cmd(&["seek", "20.500", "absolute+exact"])
        );
    }
    #[test]
    fn group_hook_holds_at_the_end_and_loads_paused() {
        assert_eq!(
            group_hook_properties(Duration::from_millis(20_500)),
            vec![
                ("file-local-options/pause", "yes".to_owned()),
                ("file-local-options/keep-open", "always".to_owned()),
                ("file-local-options/keep-open-pause", "no".to_owned()),
                ("file-local-options/start", "20.500".to_owned()),
            ]
        );
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
}
