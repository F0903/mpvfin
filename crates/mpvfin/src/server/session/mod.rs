//! The session supervisor (login, registration, WebSocket) and the job worker.
//!
//! All run on their own threads; the caller's thread only queues jobs and
//! sets flags, so no network I/O or sleep ever runs on it. `SyncPlay` requests
//! get a second job thread, so they never wait behind a report or a resolve.

mod group;
mod jobs;
mod supervisor;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use jellyfin::{BaseUrl, Client, GroupRequest, GroupSummary, Secret};

use crate::next_up::thumbnail::Thumbnail;
use crate::receiver::{Generation, PlaybackSnapshot, ReceiverEvent};
use crate::server::session::group::keep_clock;
use crate::server::session::jobs::work;
use crate::server::session::supervisor::supervise;

/// Where the plugin's files live.
#[derive(Debug)]
pub struct Paths {
    /// `script-opts/mpvfin.conf`.
    pub config_file: PathBuf,
    /// `script-opts/mpvfin-state.json`; its lock makes one window the cast target.
    pub state_file: PathBuf,
}

/// What the session asks of the plugin thread.
#[derive(Debug)]
pub enum SessionEvent {
    /// Feed to the reducer.
    Receiver(ReceiverEvent),
    /// Show on mpv's OSD.
    Osd {
        /// Message text.
        text: String,
        /// How long to show it.
        duration: Duration,
    },
    /// Write to mpv's log; the plugin adds the `[mpvfin] ` prefix.
    Log(String),
    /// The answer to [`Job::ListGroups`].
    GroupList(Vec<GroupSummary>),
    /// [`Job::ListGroups`] failed; the viewer has been told.
    GroupListFailed,
    /// The answer to [`Job::NextUpCard`].
    NextUpCard {
        /// The item asked about.
        item_id: String,
        /// What the Next Up card should say.
        label: String,
        /// The item's picture, if it has one.
        thumbnail: Option<Thumbnail>,
    },
}

/// Receives session events from the session threads; must not block.
pub type Sink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// A playback lifecycle report.
#[derive(Debug)]
pub enum Report {
    /// `POST /Sessions/Playing`.
    Started(PlaybackSnapshot),
    /// `POST /Sessions/Playing/Progress`.
    Progress(PlaybackSnapshot),
    /// `POST /Sessions/Playing/Stopped`.
    Stopped {
        /// Final state.
        snapshot: PlaybackSnapshot,
        /// Playback ended in an error.
        failed: bool,
    },
}

/// Work for the job threads, each running its jobs in submission order.
#[derive(Debug)]
pub enum Job {
    /// Negotiate playback; answered with `MediaResolved` or `MediaResolutionFailed`.
    Resolve {
        /// Echoed in the answer.
        generation: Generation,
        /// Jellyfin item id.
        item_id: String,
    },
    /// Post a playback report.
    Report(Report),
    /// `POST /UserPlayedItems/{id}`; dropped when logged out.
    MarkPlayed {
        /// Jellyfin item id.
        item_id: String,
    },
    /// Send a `SyncPlay` request; runs on the group thread.
    Group(GroupRequest),
    /// `GET /SyncPlay/List`, answered with `GroupList`; runs on the group thread.
    ListGroups,
    /// Look up an item's title and picture; always answered with `NextUpCard`.
    NextUpCard {
        /// Jellyfin item id.
        item_id: String,
    },
}

/// The running session; dropping it without `shutdown` detaches the threads.
pub struct SessionHandle {
    jobs: Sender<Job>,
    group_jobs: Sender<Job>,
    stop: Arc<AtomicBool>,
    worker_done: Receiver<()>,
    group_done: Receiver<()>,
    supervisor_done: Receiver<()>,
}

impl SessionHandle {
    /// Queues a job; never blocks.
    pub fn submit(&self, job: Job) {
        let queue = match job {
            Job::Group(_) | Job::ListGroups => &self.group_jobs,
            _ => &self.jobs,
        };
        let _ = queue.send(job);
    }

    /// Runs queued jobs for up to `flush`, then closes the WebSocket; returns
    /// within `flush` plus a second. Threads still stuck in a request are
    /// detached; they own everything they use.
    pub fn shutdown(self, flush: Duration) {
        let Self {
            jobs,
            group_jobs,
            stop,
            worker_done,
            group_done,
            supervisor_done,
        } = self;
        let started = Instant::now();
        // Both job threads exit once their queues are empty.
        drop(jobs);
        drop(group_jobs);
        let _ = worker_done.recv_timeout(flush);
        let _ = group_done.recv_timeout(flush.saturating_sub(started.elapsed()));
        stop.store(true, Ordering::Relaxed); // also makes the workers drop what is left
        // Unused flush time goes to the socket close (reads wake every 250 ms).
        let budget = flush.saturating_add(Duration::from_secs(1));
        let _ = supervisor_done.recv_timeout(budget.saturating_sub(started.elapsed()));
    }
}

/// Credentials the job worker uses; set by the supervisor after registration.
#[derive(Clone)]
struct Creds {
    client: Client,
    user_id: String,
    /// The `Authorization` value the media proxy sends upstream.
    auth: Secret,
    /// The media proxy; every URL given to mpv goes through it.
    media_base: BaseUrl,
}

/// The `SyncPlay` membership and clock, shared by the session threads.
#[derive(Default)]
struct GroupState {
    /// The group to rejoin after a reconnect.
    joined: Option<String>,
    /// The server has us in a group on the current socket.
    in_group: bool,
    /// Server clock minus local clock.
    offset_ms: i64,
}

/// Starts the supervisor, job, group and clock threads and returns at once.
#[must_use]
pub fn start(paths: Paths, sink: Sink) -> SessionHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let creds = Arc::new(Mutex::new(None));
    let group = Arc::new(Mutex::new(GroupState::default()));
    let (jobs, queue) = mpsc::channel();
    let (group_jobs, group_queue) = mpsc::channel();
    let (clock_wake, clock_woken) = mpsc::channel();
    let (supervisor_tx, supervisor_done) = mpsc::channel();
    let (worker_tx, worker_done) = mpsc::channel();
    let (group_tx, group_done) = mpsc::channel();
    let (s, st, c, g) = (sink.clone(), stop.clone(), creds.clone(), group.clone());
    spawn("jobs", &sink, move || {
        work(&queue, &s, &st, &c, &g);
        drop(worker_tx); // also dropped on unwind: `recv_timeout` sees the thread end
    });
    let (s, st, c, g) = (sink.clone(), stop.clone(), creds.clone(), group.clone());
    spawn("group", &sink, move || {
        work(&group_queue, &s, &st, &c, &g);
        drop(group_tx);
    });
    let (s, st, c, g) = (sink.clone(), stop.clone(), creds.clone(), group.clone());
    spawn("clock", &sink, move || {
        keep_clock(&clock_woken, &s, &st, &c, &g);
    });
    let (s, st) = (sink.clone(), stop.clone());
    spawn("session", &s, move || {
        supervise(&paths, &sink, &st, &creds, &group, &clock_wake);
        drop(supervisor_tx);
    });
    SessionHandle {
        jobs,
        group_jobs,
        stop,
        worker_done,
        group_done,
        supervisor_done,
    }
}

fn spawn(name: &str, sink: &Sink, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = thread::Builder::new()
        .name(format!("mpvfin-{name}"))
        .spawn(f)
    {
        log(sink, format!("cannot start the {name} thread: {e}"));
    }
}

fn log(sink: &Sink, text: String) {
    sink(SessionEvent::Log(text));
}

fn osd(sink: &Sink, text: &str, secs: u64) {
    sink(SessionEvent::Osd {
        text: text.to_owned(),
        duration: Duration::from_secs(secs),
    });
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn set_creds(creds: &Mutex<Option<Creds>>, value: Option<Creds>) {
    *locked(creds) = value;
}

/// Sleeps in 1 s slices; false as soon as `stop` is set.
fn sleep(stop: &AtomicBool, duration: Duration) -> bool {
    let end = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(Duration::from_secs(1)));
    }
    false
}

#[cfg(test)]
mod tests;
