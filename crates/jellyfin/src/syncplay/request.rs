//! Requests a member sends to its group.

use std::time::Duration;

use serde_json::{Value, json};

use crate::client::{Client, HttpError};
use crate::time::duration_to_ticks;
use crate::time::format_utc;

/// A request a member sends to the group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupRequest {
    /// Create a group.
    New {
        /// Group name.
        name: String,
    },
    /// Join a group.
    Join {
        /// Group id.
        group_id: String,
    },
    /// Leave the group.
    Leave,
    /// Replace the queue.
    SetNewQueue {
        /// Items in order.
        item_ids: Vec<String>,
        /// Index to play.
        index: usize,
        /// Start position.
        start: Duration,
    },
    /// Add to the queue.
    Queue {
        /// Items to add.
        item_ids: Vec<String>,
        /// Insert after the current item instead of at the end.
        next: bool,
    },
    /// Jump to an entry.
    SetPlaylistItem {
        /// Entry id.
        pid: String,
    },
    /// Advance from an entry.
    NextItem {
        /// Entry id.
        pid: String,
    },
    /// Go back from an entry.
    PreviousItem {
        /// Entry id.
        pid: String,
    },
    /// Resume the group.
    Unpause,
    /// Pause the group.
    Pause,
    /// Stop the group.
    Stop,
    /// Seek the group.
    Seek {
        /// Target position.
        position: Duration,
    },
    /// Report ready at a position.
    Ready {
        /// Sampled position.
        position: Duration,
        /// Whether local playback is running.
        is_playing: bool,
        /// Entry id.
        pid: String,
    },
    /// Report buffering at a position.
    Buffering {
        /// Sampled position.
        position: Duration,
        /// Entry id.
        pid: String,
    },
}

/// The HTTP path (with the `/SyncPlay/` prefix) and JSON body for a request.
/// `server_now_ms` stamps `Ready` and `Buffering`. Bodiless requests give
/// `Value::Null`.
#[must_use]
pub fn request_path_and_body(request: &GroupRequest, server_now_ms: i64) -> (&'static str, Value) {
    let report = |position: &Duration, is_playing: bool, pid: &str| {
        json!({
            "When": format_utc(server_now_ms),
            "PositionTicks": duration_to_ticks(*position),
            "IsPlaying": is_playing,
            "PlaylistItemId": pid,
        })
    };
    let pid_body = |pid: &str| json!({ "PlaylistItemId": pid });
    match request {
        GroupRequest::New { name } => ("/SyncPlay/New", json!({ "GroupName": name })),
        GroupRequest::Join { group_id } => ("/SyncPlay/Join", json!({ "GroupId": group_id })),
        GroupRequest::Leave => ("/SyncPlay/Leave", Value::Null),
        GroupRequest::SetNewQueue {
            item_ids,
            index,
            start,
        } => (
            "/SyncPlay/SetNewQueue",
            json!({
                "PlayingQueue": item_ids,
                "PlayingItemPosition": index,
                "StartPositionTicks": duration_to_ticks(*start),
            }),
        ),
        GroupRequest::Queue { item_ids, next } => (
            "/SyncPlay/Queue",
            json!({
                "ItemIds": item_ids,
                "Mode": if *next { "QueueNext" } else { "Queue" },
            }),
        ),
        GroupRequest::SetPlaylistItem { pid } => ("/SyncPlay/SetPlaylistItem", pid_body(pid)),
        GroupRequest::NextItem { pid } => ("/SyncPlay/NextItem", pid_body(pid)),
        GroupRequest::PreviousItem { pid } => ("/SyncPlay/PreviousItem", pid_body(pid)),
        GroupRequest::Unpause => ("/SyncPlay/Unpause", Value::Null),
        GroupRequest::Pause => ("/SyncPlay/Pause", Value::Null),
        GroupRequest::Stop => ("/SyncPlay/Stop", Value::Null),
        GroupRequest::Seek { position } => (
            "/SyncPlay/Seek",
            json!({ "PositionTicks": duration_to_ticks(*position) }),
        ),
        GroupRequest::Ready {
            position,
            is_playing,
            pid,
        } => ("/SyncPlay/Ready", report(position, *is_playing, pid)),
        GroupRequest::Buffering { position, pid } => {
            ("/SyncPlay/Buffering", report(position, true, pid))
        }
    }
}

impl Client {
    /// Sends a group request, stamped with the server's time now.
    ///
    /// # Errors
    /// The request failed.
    pub fn syncplay(&self, request: &GroupRequest, server_now_ms: i64) -> Result<(), HttpError> {
        let (path, body) = request_path_and_body(request, server_now_ms);
        self.post(path, &body).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_maps_to_its_path_and_body() {
        use GroupRequest as R;
        let s = |x: &str| x.to_owned();
        let pos = Duration::from_secs(3);
        let now = 1_767_323_045_007;
        let when = "2026-01-02T03:04:05.007Z";
        let ready = |playing: bool| json!({"When": when, "PositionTicks": 30_000_000, "IsPlaying": playing, "PlaylistItemId": "p"});
        let pid = json!({"PlaylistItemId": "p"});
        let table: Vec<(R, &str, Value)> = vec![
            (
                R::New { name: s("n") },
                "/SyncPlay/New",
                json!({"GroupName": "n"}),
            ),
            (
                R::Join { group_id: s("g") },
                "/SyncPlay/Join",
                json!({"GroupId": "g"}),
            ),
            (R::Leave, "/SyncPlay/Leave", Value::Null),
            (
                R::SetNewQueue {
                    item_ids: vec![s("a"), s("b")],
                    index: 1,
                    start: pos,
                },
                "/SyncPlay/SetNewQueue",
                json!({"PlayingQueue": ["a", "b"], "PlayingItemPosition": 1, "StartPositionTicks": 30_000_000}),
            ),
            (
                R::Queue {
                    item_ids: vec![s("a")],
                    next: false,
                },
                "/SyncPlay/Queue",
                json!({"ItemIds": ["a"], "Mode": "Queue"}),
            ),
            (
                R::Queue {
                    item_ids: vec![s("a")],
                    next: true,
                },
                "/SyncPlay/Queue",
                json!({"ItemIds": ["a"], "Mode": "QueueNext"}),
            ),
            (
                R::SetPlaylistItem { pid: s("p") },
                "/SyncPlay/SetPlaylistItem",
                pid.clone(),
            ),
            (
                R::NextItem { pid: s("p") },
                "/SyncPlay/NextItem",
                pid.clone(),
            ),
            (
                R::PreviousItem { pid: s("p") },
                "/SyncPlay/PreviousItem",
                pid,
            ),
            (R::Unpause, "/SyncPlay/Unpause", Value::Null),
            (R::Pause, "/SyncPlay/Pause", Value::Null),
            (R::Stop, "/SyncPlay/Stop", Value::Null),
            (
                R::Seek { position: pos },
                "/SyncPlay/Seek",
                json!({"PositionTicks": 30_000_000}),
            ),
            (
                R::Ready {
                    position: pos,
                    is_playing: false,
                    pid: s("p"),
                },
                "/SyncPlay/Ready",
                ready(false),
            ),
            (
                R::Buffering {
                    position: pos,
                    pid: s("p"),
                },
                "/SyncPlay/Buffering",
                ready(true),
            ),
        ];
        assert_eq!(table.len(), 15);
        for (req, path, body) in table {
            assert_eq!(request_path_and_body(&req, now), (path, body), "{req:?}");
        }
    }
}
