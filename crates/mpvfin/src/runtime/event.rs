//! mpv's events, copied out of its memory, and the key presses among its messages.

use std::ffi::{c_char, c_int};

use super::mpv::ffi::{
    MPV_EVENT_CLIENT_MESSAGE, MPV_EVENT_COMMAND_REPLY, MPV_EVENT_END_FILE, MPV_EVENT_FILE_LOADED,
    MPV_EVENT_HOOK, MPV_EVENT_PLAYBACK_RESTART, MPV_EVENT_PROPERTY_CHANGE, MPV_EVENT_SEEK,
    MPV_EVENT_SHUTDOWN, MPV_EVENT_START_FILE, MPV_FORMAT_STRING, MpvEvent, MpvEventClientMessage,
    MpvEventEndFile, MpvEventHook, MpvEventProperty, MpvEventStartFile,
};
use super::mpv::from_c;

/// The parts of an mpv event the runtime uses, copied out of mpv's memory.
pub(super) enum Event {
    StartFile(i64),
    Hook(u64),
    FileLoaded,
    EndFile {
        entry_id: i64,
        reason: c_int,
        error: c_int,
    },
    Seek,
    Restart,
    Property(String, Option<String>),
    ClientMessage(Vec<String>),
    CommandReply {
        reply_id: u64,
        error: c_int,
    },
    Shutdown,
    /// A wakeup, a timeout, or an event the runtime does not use.
    Other,
}

impl Event {
    pub(super) fn read(event: &MpvEvent) -> Self {
        let data = event.data;
        // SAFETY: client.h fixes `data`'s type for each event id (`as_ref`
        // covers NULL), and mpv keeps it valid until the next wait, which
        // cannot happen while `event` is borrowed.
        unsafe {
            match event.event_id {
                MPV_EVENT_START_FILE => data
                    .cast::<MpvEventStartFile>()
                    .as_ref()
                    .map_or(Self::Other, |e| Self::StartFile(e.playlist_entry_id)),
                // `on_load` is the only hook registered.
                MPV_EVENT_HOOK => data
                    .cast::<MpvEventHook>()
                    .as_ref()
                    .map_or(Self::Other, |e| Self::Hook(e.id)),
                MPV_EVENT_FILE_LOADED => Self::FileLoaded,
                MPV_EVENT_END_FILE => {
                    data.cast::<MpvEventEndFile>()
                        .as_ref()
                        .map_or(Self::Other, |e| Self::EndFile {
                            entry_id: e.playlist_entry_id,
                            reason: e.reason,
                            error: e.error,
                        })
                }
                MPV_EVENT_SEEK => Self::Seek,
                MPV_EVENT_PLAYBACK_RESTART => Self::Restart,
                MPV_EVENT_PROPERTY_CHANGE => data
                    .cast::<MpvEventProperty>()
                    .as_ref()
                    .and_then(|p| {
                        // Format NONE: the property is unavailable.
                        let value = (p.format == MPV_FORMAT_STRING)
                            .then(|| p.data.cast::<*const c_char>().as_ref())
                            .flatten()
                            .and_then(|s| from_c(*s));
                        Some(Self::Property(from_c(p.name)?, value))
                    })
                    .unwrap_or(Self::Other),
                MPV_EVENT_CLIENT_MESSAGE => {
                    data.cast::<MpvEventClientMessage>()
                        .as_ref()
                        .map_or(Self::Other, |m| {
                            let count = usize::try_from(m.num_args).unwrap_or(0);
                            // Non-UTF-8 becomes empty, so positions stay meaningful.
                            Self::ClientMessage(
                                (0..count)
                                    .map(|i| from_c(*m.args.add(i)).unwrap_or_default())
                                    .collect(),
                            )
                        })
                }
                MPV_EVENT_COMMAND_REPLY => Self::CommandReply {
                    reply_id: event.reply_userdata,
                    error: event.error,
                },
                MPV_EVENT_SHUTDOWN => Self::Shutdown,
                _ => Self::Other,
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
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
}
