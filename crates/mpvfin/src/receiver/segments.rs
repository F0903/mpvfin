//! When the skip button and the Next Up card show, from the item's segments
//! and position.

use std::time::Duration;

use jellyfin::SegmentKind;

use crate::receiver::Receiver;
use crate::receiver::action::ReceiverAction;
use crate::receiver::playback::{Generation, PlaybackState};

/// How long before the end the Next Up card shows on an item without credits.
pub(super) const NEXT_UP_LEAD: Duration = Duration::from_secs(30);

/// Where the Next Up card stands for the current item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NextUpState {
    /// Playback is outside the card's window.
    Hidden,
    /// The card is due.
    Shown,
    /// Play Now was pressed; no card until playback leaves the window.
    Taken,
}

impl Receiver {
    /// Shows, swaps or hides the skip button for the current position.
    pub(super) fn evaluate_segments(&mut self, actions: &mut Vec<ReceiverAction>) {
        let PlaybackState::Playing(snapshot) = &self.playback else {
            return;
        };
        if !self.position_settled {
            return;
        }
        let (generation, position) = (snapshot.generation, snapshot.position);
        let found = self
            .segments
            .iter()
            .find(|s| s.start <= position && position < s.end)
            .cloned();
        if self.dismissed.is_some() && self.dismissed != found {
            self.dismissed = None;
        }
        let next = found.filter(|s| self.dismissed.as_ref() != Some(s));
        if next != self.active {
            if self.active.is_some() {
                actions.push(ReceiverAction::HideSkipButton { generation });
            }
            if let Some(segment) = &next {
                actions.push(ReceiverAction::ShowSkipButton {
                    generation,
                    kind: segment.kind,
                });
            }
            self.active = next;
        }
        self.evaluate_next_up(generation, position, actions);
    }

    /// Shows or hides the Next Up card for the current position.
    pub(super) fn evaluate_next_up(
        &mut self,
        generation: Generation,
        position: Duration,
        actions: &mut Vec<ReceiverAction>,
    ) {
        let start = self
            .segments
            .iter()
            .find(|s| s.kind == SegmentKind::Outro)
            .map(|s| s.start)
            .or_else(|| self.duration.map(|d| d.saturating_sub(NEXT_UP_LEAD)));
        let due = start.is_some_and(|start| position >= start);
        let state = match (due, self.next_up) {
            (false, _) => NextUpState::Hidden,
            (true, NextUpState::Taken) => NextUpState::Taken,
            (true, _) => NextUpState::Shown,
        };
        match (self.next_up, state) {
            (NextUpState::Shown, NextUpState::Hidden) => {
                actions.push(ReceiverAction::HideNextUp { generation });
            }
            (NextUpState::Hidden, NextUpState::Shown) => {
                actions.push(ReceiverAction::ShowNextUp { generation });
            }
            _ => {}
        }
        self.next_up = state;
    }
}
