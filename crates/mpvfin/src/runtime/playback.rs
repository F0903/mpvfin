//! Following an entry from mpv's load hook to its end, and what mpv reports
//! about it on the way.

use std::ffi::c_int;

use super::Runtime;
use super::load::{
    SubtitleStep, SubtitleWait, TrackKind, is_true_end, selection_commands, sub_add_commands,
    track_command,
};
use super::overlays::UOSC_MENU;
use super::playlist::entry_from_uri;
use crate::receiver::{Generation, PlayerToken, ReceiverEvent};
use crate::runtime::values::{osd_length, parse_position, percent};

impl Runtime {
    pub(super) fn on_load(&mut self, hook_id: u64) {
        let filename = self.mpv.get_property("stream-open-filename");
        let entry = filename.as_deref().and_then(entry_from_uri);
        if let (Some(entry_id), Some((item_id, pid))) = (self.current, entry) {
            self.feed(ReceiverEvent::PlayerOpening {
                token: PlayerToken { entry_id, hook_id },
                item_id: item_id.to_owned(),
                playlist_item_id: pid.map(str::to_owned),
            });
        } else {
            // Not an mpvfin entry: mpv must not wait for us.
            self.mpv.hook_continue(hook_id);
            self.feed(ReceiverEvent::PlayerOpenedForeign);
        }
    }

    pub(super) fn file_loaded(&mut self) {
        let Some(entry) = self.current.and_then(|id| self.entries.get(&id)) else {
            return;
        };
        let generation = entry.generation;
        let (sub_adds, selection) = entry
            .media
            .as_ref()
            .map(|(media, selection)| {
                (
                    sub_add_commands(media),
                    selection_commands(media, selection),
                )
            })
            .unwrap_or_default();
        self.feed(ReceiverEvent::PlayerFileLoaded { generation });
        self.subtitle_wait = Some(SubtitleWait::new(generation, sub_adds, selection));
        self.advance_subtitles();
        // mpv delivers events ahead of property changes, and two items of the
        // same length would send no change at all.
        self.observed.duration = parse_position(self.mpv.get_property("duration").as_deref());
        for name in ["pause", "volume", "mute", "aid", "duration"] {
            if let Some(event) = self.player_event(generation, name) {
                self.feed(event);
            }
        }
    }

    /// Issues the next `sub-add` off the plugin thread (a stalled subtitle must
    /// not freeze it), or applies the selection once none remain.
    pub(super) fn advance_subtitles(&mut self) {
        while let Some(wait) = self.subtitle_wait.as_mut() {
            let generation = wait.generation();
            match wait.step() {
                SubtitleStep::Add(command) => {
                    let args: Vec<&str> = command.iter().map(String::as_str).collect();
                    match self.mpv.command_async(generation.get(), &args) {
                        Ok(()) => return,
                        Err(e) => self.mpv.log(&e),
                    }
                }
                SubtitleStep::Select(selection) => {
                    self.subtitle_wait = None;
                    for command in selection {
                        self.run(&command);
                    }
                }
            }
        }
    }

    pub(super) fn sub_add_reply(&mut self, reply_id: u64, error: c_int) {
        if error < 0 {
            self.mpv
                .log(&format!("sub-add: {}", self.mpv.error_string(error)));
        }
        let current = self.current_generation();
        if self
            .subtitle_wait
            .as_ref()
            .is_some_and(|wait| wait.accepts(reply_id) && Some(wait.generation()) == current)
        {
            self.advance_subtitles();
        }
    }

    pub(super) fn property(&mut self, name: &str, value: Option<String>) {
        let o = &mut self.observed;
        match (name, value.as_deref()) {
            ("pause", Some(v)) => o.paused = v == "yes",
            ("mute", Some(v)) => o.muted = v == "yes",
            ("volume", Some(v)) => {
                if let Ok(volume) = v.parse() {
                    o.volume_percent = percent(volume);
                }
            }
            ("time-pos", v) => {
                let position = parse_position(v);
                o.position = position.unwrap_or_default();
                if position.is_none() {
                    // No file: the reducer keeps the last real position for its stop report.
                    return;
                }
            }
            ("aid", _) => o.aid = value,
            ("sid", _) => o.sid = value,
            ("duration", v) => o.duration = parse_position(v),
            ("osd-width", v) => {
                o.osd_width = osd_length(v);
                return self.overlays_resized();
            }
            ("osd-height", v) => {
                o.osd_height = osd_length(v);
                return self.overlays_resized();
            }
            ("mouse-pos" | UOSC_MENU, _) => return self.update_hover(),
            ("speed", v) => {
                if let Some(speed) = v.and_then(|v| v.parse().ok()) {
                    o.speed = speed;
                }
                return;
            }
            ("eof-reached", v) => {
                o.eof_reached = v == Some("yes");
                if o.eof_reached
                    && let Some(generation) = self.current_generation()
                {
                    self.feed(self.end_event(generation));
                }
                return;
            }
            ("paused-for-cache", v) => {
                if let Some(generation) = self.current_generation() {
                    self.feed(ReceiverEvent::PlayerBuffering {
                        generation,
                        buffering: v == Some("yes"),
                    });
                }
                return;
            }
            ("idle-active", Some("yes")) => return self.feed(ReceiverEvent::PlayerIdle),
            _ => return,
        }
        if let Some(event) = self
            .current_generation()
            .and_then(|generation| self.player_event(generation, name))
        {
            self.feed(event);
        }
        if name == "time-pos" {
            self.correct_drift();
        }
    }

    pub(super) fn end_event(&self, generation: Generation) -> ReceiverEvent {
        // mpv delivers events ahead of property changes, so ask for the position.
        let position = parse_position(self.mpv.get_property("time-pos").as_deref())
            .unwrap_or(self.observed.position);
        let duration = parse_position(self.mpv.get_property("duration").as_deref());
        if is_true_end(position, duration) {
            ReceiverEvent::PlayerReachedEnd { generation }
        } else {
            ReceiverEvent::PlayerStoppedEarly { generation }
        }
    }

    /// The `Player*Changed` event carrying the latest value of property `name`.
    pub(super) fn player_event(&self, generation: Generation, name: &str) -> Option<ReceiverEvent> {
        let o = &self.observed;
        Some(match name {
            "pause" => ReceiverEvent::PlayerPauseChanged {
                generation,
                paused: o.paused,
            },
            "volume" => ReceiverEvent::PlayerVolumeChanged {
                generation,
                volume_percent: o.volume_percent,
            },
            "mute" => ReceiverEvent::PlayerMuteChanged {
                generation,
                muted: o.muted,
            },
            "time-pos" => ReceiverEvent::PlayerPositionChanged {
                generation,
                position: o.position,
                seek_completed: false,
            },
            "duration" => ReceiverEvent::PlayerDurationChanged {
                generation,
                duration: o.duration,
            },
            "aid" | "sid" => {
                let tracks = &self.media(generation)?.0.tracks;
                // "no" and "auto" are not track ids, so they map to `None`.
                let id = |value: &Option<String>| value.as_deref()?.parse::<i64>().ok();
                ReceiverEvent::PlayerTracksChanged {
                    generation,
                    audio_stream_index: id(&o.aid).and_then(|id| tracks.audio_index(id)),
                    subtitle_stream_index: id(&o.sid).and_then(|id| tracks.subtitle_index(id)),
                }
            }
            _ => return None,
        })
    }

    pub(super) fn select_track(&self, generation: Generation, kind: TrackKind, index: i32) {
        let command = self
            .media(generation)
            .and_then(|(media, _)| track_command(media, kind, index));
        match command {
            Some(command) => self.run(&command),
            None => self.mpv.log(&format!(
                "no mpv track for Jellyfin {kind:?} stream {index}"
            )),
        }
    }
}
