//! The job threads: resolves, segments, reports, played state and the Next Up card.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;

use crate::next_up::thumbnail::{Thumbnail, decode_thumbnail};
use crate::receiver::ReceiverEvent;
use crate::server::session::group::{list_groups, send_group_request};
use crate::server::session::{
    Creds, GroupState, Job, Report, SessionEvent, Sink, locked, log, spawn,
};

pub(super) fn work(
    queue: &Receiver<Job>,
    sink: &Sink,
    stop: &AtomicBool,
    creds: &Mutex<Option<Creds>>,
    group: &Mutex<GroupState>,
) {
    for job in queue {
        if stop.load(Ordering::Relaxed) {
            return; // the shutdown flush window is over
        }
        let current = locked(creds).clone();
        match job {
            Job::Group(request) => {
                send_group_request(&request, current.as_ref(), sink, stop, group);
            }
            Job::ListGroups => list_groups(current, sink),
            Job::Resolve {
                generation,
                item_id,
            } => {
                // Always answered: an unanswered resolve would hold mpv's load hook forever.
                let result = current
                    .as_ref()
                    .ok_or_else(|| "not connected to Jellyfin".to_owned())
                    .and_then(|c| c.client.resolve(&c.user_id, &item_id, &c.media_base));
                let (event, runtime) = match result {
                    Ok((media, runtime)) => {
                        (ReceiverEvent::MediaResolved { generation, media }, runtime)
                    }
                    Err(message) => (
                        ReceiverEvent::MediaResolutionFailed {
                            generation,
                            message,
                        },
                        None,
                    ),
                };
                let resolved = matches!(event, ReceiverEvent::MediaResolved { .. });
                sink(SessionEvent::Receiver(event));
                // After MediaResolved, so playback never waits for segments; on
                // its own thread, so a stalled reply can't delay the next resolve.
                if let (true, Some(c)) = (resolved, current) {
                    let s = sink.clone();
                    spawn("segments", sink, move || {
                        match c.client.segments(&item_id, runtime) {
                            Ok(segments) => {
                                s(SessionEvent::Receiver(ReceiverEvent::SegmentsResolved {
                                    generation,
                                    segments,
                                }));
                            }
                            Err(e) => log(&s, format!("media segments unavailable: {e}")),
                        }
                    });
                }
            }
            // On its own thread, so a stalled reply can't delay the next item's resolve.
            Job::NextUpCard { item_id } => {
                let s = sink.clone();
                spawn("next-up", sink, move || {
                    let label = next_up_label(current.as_ref(), &item_id, &s);
                    let thumbnail = next_up_thumbnail(current.as_ref(), &item_id);
                    s(SessionEvent::NextUpCard {
                        item_id,
                        label,
                        thumbnail,
                    });
                });
            }
            Job::MarkPlayed { item_id } => {
                let Some(c) = current else {
                    continue;
                };
                if let Err(e) = c.client.mark_played(&c.user_id, &item_id) {
                    log(sink, format!("marking the item played failed: {e}"));
                }
            }
            Job::Report(report) => {
                let Some(c) = current else {
                    continue; // no credentials: dropped
                };
                let sent = match &report {
                    Report::Started(s) => c.client.report_started(&s.into()),
                    Report::Progress(s) => c.client.report_progress(&s.into()),
                    Report::Stopped { snapshot, failed } => {
                        c.client.report_stopped(&snapshot.into(), *failed)
                    }
                };
                if let Err(e) = sent {
                    log(sink, format!("playback report failed: {e}"));
                }
            }
        }
    }
}

/// The item's picture. Plenty of items have none, so failing is unremarkable.
pub(super) fn next_up_thumbnail(creds: Option<&Creds>, item_id: &str) -> Option<Thumbnail> {
    let jpeg = creds?.client.thumbnail(item_id).ok()?;
    decode_thumbnail(&jpeg)
}

/// The item's short title, or a generic one: Play Now works without a title.
pub(super) fn next_up_label(creds: Option<&Creds>, item_id: &str, sink: &Sink) -> String {
    creds
        .and_then(|c| {
            c.client
                .short_title(&c.user_id, item_id)
                .map_err(|e| log(sink, format!("next item's title unavailable: {e}")))
                .ok()
        })
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "Next item".to_owned())
}
