//! Following a `SyncPlay` group: the rules of `docs/jellyfin-syncplay.md`,
//! sections 6 to 8, as reducer transitions.

mod commands;
mod membership;
mod queue;
mod viewer;

use std::time::Duration;

use jellyfin::GroupEntry;

/// Closer than this to a commanded position needs no seek.
const POSITION_TOLERANCE: Duration = Duration::from_millis(100);

/// What this session knows of its group.
// Each flag tracks a different server exchange; they are independent.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub(crate) struct Group {
    id: String,
    /// The queue in server order.
    entries: Vec<GroupEntry>,
    /// The queue's current entry, from the latest update.
    current_pid: Option<String>,
    /// Server time of the last applied queue update.
    last_update_ms: i64,
    /// The server waits for a Ready once the item settles.
    awaiting_ready: bool,
    /// The pause state the group wants.
    expected_paused: bool,
    /// Where a seek issued for the group is going, until it lands.
    own_seek: Option<Duration>,
    /// Where the group starts: a load's start, or a scheduled resume's position.
    start: Duration,
    /// The load replaces a playing item with no Unpause to come.
    resume_on_settle: bool,
    /// The server has heard that the current item ended.
    reached_end: bool,
    /// The server has heard that playback stalled.
    buffering: bool,
    /// The viewer's Pause and Unpause requests whose broadcasts are on their way.
    own_requests: u32,
    /// mpv hasn't yet confirmed the pause that holds a viewer's resume.
    hold_unconfirmed: bool,
    /// A `GroupLoad` has not reached mpv's hook yet.
    load_coming: bool,
    /// An entry the server waits on stopped; only mpv going idle makes that final.
    stopped_owing: bool,
}

impl Group {
    const fn new(id: String) -> Self {
        Self {
            id,
            entries: Vec::new(),
            current_pid: None,
            last_update_ms: 0,
            awaiting_ready: false,
            expected_paused: true,
            own_seek: None,
            start: Duration::ZERO,
            resume_on_settle: false,
            reached_end: false,
            buffering: false,
            own_requests: 0,
            hold_unconfirmed: false,
            load_coming: false,
            stopped_owing: false,
        }
    }
}

#[cfg(test)]
mod tests;
