//! Pure translation between the core's types and mpv commands.

use std::ffi::c_int;
use std::time::Duration;

use mpvfin_core::{
    EndReason, Generation, GroupEntry, QueueMode, QueueRequest, ResolvedMedia, StreamSelection,
};

use crate::ffi::{MPV_END_FILE_REASON_EOF, MPV_END_FILE_REASON_ERROR};

/// URL scheme of the playlist entries mpvfin queues.
pub const ENTRY_SCHEME: &str = "mpvfin://";

const NO_RESUME: &str = "resume-playback=no";

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

/// Playlist entry URL for an entry of a `SyncPlay` queue.
#[must_use]
pub fn group_entry_uri(item_id: &str, pid: &str) -> String {
    format!("{ENTRY_SCHEME}{item_id}/{pid}")
}

/// Jellyfin item ID of an mpvfin playlist entry and the `SyncPlay` queue entry
/// it names, if any; `None` for any other path.
#[must_use]
pub fn entry_from_uri(uri: &str) -> Option<(&str, Option<&str>)> {
    let rest = uri.strip_prefix(ENTRY_SCHEME)?;
    let (id, pid) = rest.split_once('/').unwrap_or((rest, ""));
    (!id.is_empty()).then_some((id, Some(pid).filter(|pid| !pid.is_empty())))
}

fn loadfile(uri: String, flag: &str, index: String, options: String) -> Vec<String> {
    vec!["loadfile".to_owned(), uri, flag.to_owned(), index, options]
}

/// Commands queuing `uris` around `uris[current]`, the entry already in place.
fn commands_around(uris: &[String], current: usize) -> Vec<Vec<String>> {
    let later = uris[current + 1..]
        .iter()
        .map(|uri| loadfile(uri.clone(), "append", "-1".into(), NO_RESUME.into()));
    let earlier = uris[..current]
        .iter()
        .enumerate()
        .map(|(i, uri)| loadfile(uri.clone(), "insert-at", i.to_string(), NO_RESUME.into()));
    later.chain(earlier).collect()
}

fn group_uris(entries: &[GroupEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| group_entry_uri(&e.item_id, &e.playlist_item_id))
        .collect()
}

/// Replaces mpv's playlist with the group's queue, opening `entries[index]`.
/// The start position is the hook's: the group can move it until then.
#[must_use]
pub fn group_load_commands(entries: &[GroupEntry], index: usize) -> Vec<Vec<String>> {
    let uris = group_uris(entries);
    let current = uris[index].clone();
    let mut commands = vec![loadfile(current, "replace", "-1".into(), NO_RESUME.into())];
    commands.extend(commands_around(&uris, index));
    commands
}

/// Mirrors the group's queue around the playing entry, which `playlist-clear` keeps.
#[must_use]
pub fn group_sync_commands(entries: &[GroupEntry], index: usize) -> Vec<Vec<String>> {
    let mut commands = vec![vec!["playlist-clear".to_owned()]];
    commands.extend(commands_around(&group_uris(entries), index));
    commands
}

/// What a new group queue takes from mpv's playlist: the mpvfin entries' item
/// ids, and the index among them of the entry at `current`.
#[must_use]
pub fn published_queue(filenames: &[String], current: usize) -> (Vec<String>, usize) {
    let item_ids = |names: &[String]| -> Vec<String> {
        names
            .iter()
            .filter_map(|name| entry_from_uri(name))
            .map(|(item_id, _)| item_id.to_owned())
            .collect()
    };
    let before = &filenames[..current.min(filenames.len())];
    (item_ids(filenames), item_ids(before).len())
}

/// The binding a client message reports as pressed: state `d` is a key going
/// down, `p` a press whose up and down can't be told apart.
#[must_use]
pub fn key_press_name(args: &[String]) -> Option<&str> {
    match args {
        [message, name, state, ..] if message == "key-binding" && state.starts_with(['d', 'p']) => {
            Some(name)
        }
        _ => None,
    }
}

/// `loadfile` commands that queue the request's items.
#[must_use]
pub fn queue_commands(req: &QueueRequest) -> Vec<Vec<String>> {
    let load = |id: &str, flag: &str, index: String, options: String| {
        loadfile(entry_uri(id), flag, index, options)
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
            let uris: Vec<String> = ids.iter().map(|id| entry_uri(id)).collect();
            commands.extend(commands_around(&uris, start));
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
    use crate::ffi::{
        MPV_END_FILE_REASON_QUIT, MPV_END_FILE_REASON_REDIRECT, MPV_END_FILE_REASON_STOP,
    };
    use mpvfin_core::{Generation, GroupEntry, PlayMethod, TrackMap};

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
    fn entries_are_recognised_and_foreign_paths_are_not() {
        assert_eq!(entry_uri("abc"), "mpvfin://abc");
        assert_eq!(entry_from_uri("mpvfin://abc"), Some(("abc", None)));
        assert_eq!(entry_from_uri(r"C:\v.mkv"), None);
        assert_eq!(entry_from_uri("https://x/y"), None);
    }

    fn group_entries(names: &[&str]) -> Vec<GroupEntry> {
        names
            .iter()
            .map(|name| GroupEntry {
                item_id: (*name).to_owned(),
                playlist_item_id: format!("p{name}"),
            })
            .collect()
    }

    fn group_cmd(uri: &str, flag: &str, index: &str) -> Vec<String> {
        cmd(&["loadfile", uri, flag, index, "resume-playback=no"])
    }

    #[test]
    fn group_uris_round_trip() {
        let uri = group_entry_uri("abc", "pabc");
        assert_eq!(uri, "mpvfin://abc/pabc");
        assert_eq!(entry_from_uri(&uri), Some(("abc", Some("pabc"))));
    }

    #[test]
    fn plain_entries_have_no_pid() {
        assert_eq!(entry_from_uri("mpvfin://abc"), Some(("abc", None)));
        assert_eq!(entry_from_uri("mpvfin://abc/"), Some(("abc", None)));
        assert_eq!(entry_from_uri("mpvfin://"), None);
        assert_eq!(entry_from_uri("mpvfin:///pabc"), None);
    }

    #[test]
    fn group_load_commands_match_the_queue_shape() {
        assert_eq!(
            group_load_commands(&group_entries(&["a", "b", "c", "d"]), 2),
            vec![
                group_cmd("mpvfin://c/pc", "replace", "-1"),
                group_cmd("mpvfin://d/pd", "append", "-1"),
                group_cmd("mpvfin://a/pa", "insert-at", "0"),
                group_cmd("mpvfin://b/pb", "insert-at", "1"),
            ]
        );
    }

    #[test]
    fn group_sync_keeps_the_current_entry() {
        assert_eq!(
            group_sync_commands(&group_entries(&["a", "b", "c", "d"]), 2),
            vec![
                cmd(&["playlist-clear"]),
                group_cmd("mpvfin://d/pd", "append", "-1"),
                group_cmd("mpvfin://a/pa", "insert-at", "0"),
                group_cmd("mpvfin://b/pb", "insert-at", "1"),
            ]
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
    fn a_published_queue_keeps_mpvfin_entries_and_finds_the_current_one() {
        let names = ["mpvfin://a", r"C:\v.mkv", "mpvfin://b/pb", "mpvfin://c"].map(str::to_owned);
        assert_eq!(
            published_queue(&names, 2),
            (vec!["a".to_owned(), "b".to_owned(), "c".to_owned()], 1)
        );
    }

    #[test]
    fn key_press_names_are_reported_only_on_press() {
        let cases: &[(&[&str], Option<&str>)] = &[
            (&["key-binding", "skip", "d--", "ENTER"], Some("skip")),
            (&["key-binding", "skip", "p--", "ENTER"], Some("skip")),
            (&["key-binding", "skip", "dm-", "MBTN_LEFT"], Some("skip")),
            (&["key-binding", "other", "d--", "ENTER"], Some("other")),
            (&["key-binding", "skip", "r--", "ENTER"], None),
            (&["key-binding", "skip", "u--", "ENTER"], None),
            (&["something-else", "skip", "d--"], None),
            (&["key-binding", "skip"], None),
            (&[], None),
        ];
        for (args, expected) in cases {
            assert_eq!(key_press_name(&cmd(args)), *expected, "{args:?}");
        }
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
