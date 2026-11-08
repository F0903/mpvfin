//! Backend-neutral mpv command and event boundary.

use std::error::Error;
use std::time::Duration;

use mpvfin_core::{Generation, ReceiverAction, ReceiverEvent, ResolvedMedia};

/// A command executed by an mpv adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlayerCommand {
    /// Load negotiated media.
    Load {
        /// Playback generation.
        generation: Generation,
        /// Negotiated media.
        media: ResolvedMedia,
        /// Requested starting position.
        start_position: Duration,
    },
    /// Set pause state.
    SetPaused {
        /// Playback generation.
        generation: Generation,
        /// Requested state.
        paused: bool,
    },
    /// Seek to an absolute position.
    Seek {
        /// Playback generation.
        generation: Generation,
        /// Requested position.
        position: Duration,
    },
    /// Set volume.
    SetVolume {
        /// Playback generation.
        generation: Generation,
        /// Requested volume, from 0 through 100.
        volume_percent: u8,
    },
    /// Set mute state.
    SetMuted {
        /// Playback generation.
        generation: Generation,
        /// Requested state.
        muted: bool,
    },
    /// Stop playback.
    Stop {
        /// Playback generation.
        generation: Generation,
    },
}

/// Converts a core side effect to a player command when applicable.
#[must_use]
pub fn command_from_action(action: &ReceiverAction) -> Option<PlayerCommand> {
    match action {
        ReceiverAction::LoadPlayer {
            generation,
            media,
            start_position,
        } => Some(PlayerCommand::Load {
            generation: *generation,
            media: media.clone(),
            start_position: *start_position,
        }),
        ReceiverAction::SetPaused { generation, paused } => Some(PlayerCommand::SetPaused {
            generation: *generation,
            paused: *paused,
        }),
        ReceiverAction::Seek {
            generation,
            position,
        } => Some(PlayerCommand::Seek {
            generation: *generation,
            position: *position,
        }),
        ReceiverAction::SetVolume {
            generation,
            volume_percent,
        } => Some(PlayerCommand::SetVolume {
            generation: *generation,
            volume_percent: *volume_percent,
        }),
        ReceiverAction::SetMuted { generation, muted } => Some(PlayerCommand::SetMuted {
            generation: *generation,
            muted: *muted,
        }),
        ReceiverAction::StopPlayer { generation } => Some(PlayerCommand::Stop {
            generation: *generation,
        }),
        _ => None,
    }
}

/// Minimal interface implemented by in-process and JSON-IPC adapters.
pub trait PlayerBackend {
    /// Adapter error type.
    type Error: Error + Send + Sync + 'static;

    /// Executes one player command.
    ///
    /// # Errors
    ///
    /// Returns the adapter's error when mpv rejects the command or the
    /// underlying client connection is unavailable.
    fn execute(&mut self, command: PlayerCommand) -> Result<(), Self::Error>;

    /// Waits for one translated player event.
    ///
    /// # Errors
    ///
    /// Returns the adapter's error when reading from mpv fails.
    fn next_event(&mut self, timeout: Duration) -> Result<Option<ReceiverEvent>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpvfin_core::{PlayMethod, ResolvedMedia};

    #[test]
    fn ignores_non_player_actions() {
        assert_eq!(
            command_from_action(&ReceiverAction::CancelMediaResolution {
                generation: Generation::new(1).expect("non-zero generation"),
            }),
            None
        );
    }

    #[test]
    fn converts_load_action_without_losing_generation() {
        let generation = Generation::new(7).expect("non-zero generation");
        let media = ResolvedMedia {
            item_id: "item".to_owned(),
            uri: "https://example.invalid/video".to_owned(),
            play_session_id: "session".to_owned(),
            media_source_id: "source".to_owned(),
            play_method: PlayMethod::DirectPlay,
        };
        let start_position = Duration::from_secs(42);

        assert_eq!(
            command_from_action(&ReceiverAction::LoadPlayer {
                generation,
                media: media.clone(),
                start_position,
            }),
            Some(PlayerCommand::Load {
                generation,
                media,
                start_position,
            })
        );
    }
}
