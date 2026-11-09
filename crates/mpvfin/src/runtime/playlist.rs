//! The `mpvfin://` entries in mpv's playlist, and the commands that queue them.

use jellyfin::{GroupEntry, QueueMode, QueueRequest};

use super::load::{NO_RESUME, format_seconds};

/// URL scheme of the playlist entries mpvfin queues.
pub const ENTRY_SCHEME: &str = "mpvfin://";

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

pub(super) fn loadfile(uri: String, flag: &str, index: String, options: String) -> Vec<String> {
    vec!["loadfile".to_owned(), uri, flag.to_owned(), index, options]
}

/// Commands queuing `uris` around `uris[current]`, the entry already in place.
pub(super) fn commands_around(uris: &[String], current: usize) -> Vec<Vec<String>> {
    let later = uris[current + 1..]
        .iter()
        .map(|uri| loadfile(uri.clone(), "append", "-1".into(), NO_RESUME.into()));
    let earlier = uris[..current]
        .iter()
        .enumerate()
        .map(|(i, uri)| loadfile(uri.clone(), "insert-at", i.to_string(), NO_RESUME.into()));
    later.chain(earlier).collect()
}

pub(super) fn group_uris(entries: &[GroupEntry]) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use jellyfin::StreamSelection;
    use std::time::Duration;

    use jellyfin::GroupEntry;

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
    fn entries_are_recognised_and_foreign_paths_are_not() {
        assert_eq!(entry_uri("abc"), "mpvfin://abc");
        assert_eq!(entry_from_uri("mpvfin://abc"), Some(("abc", None)));
        assert_eq!(entry_from_uri(r"C:\v.mkv"), None);
        assert_eq!(entry_from_uri("https://x/y"), None);
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
    fn a_published_queue_keeps_mpvfin_entries_and_finds_the_current_one() {
        let names = ["mpvfin://a", r"C:\v.mkv", "mpvfin://b/pb", "mpvfin://c"].map(str::to_owned);
        assert_eq!(
            published_queue(&names, 2),
            (vec!["a".to_owned(), "b".to_owned(), "c".to_owned()], 1)
        );
    }
}
