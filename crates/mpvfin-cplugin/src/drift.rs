//! Pure drift corrector: nudges playback speed (or seeks) so the local
//! position follows a moving anchor. No I/O; time is injected.

use std::time::{Duration, Instant};

const IGNORE_BELOW: Duration = Duration::from_millis(120);
const SEEK_ABOVE: Duration = Duration::from_secs(2);
const RATE_DELTA: f64 = 0.05;
const MAX_CORRECTION: Duration = Duration::from_secs(30);
const COOLDOWN_AFTER_SPEED: Duration = Duration::from_secs(3);
const COOLDOWN_AFTER_SEEK: Duration = Duration::from_secs(5);
/// Longer than this between two observations of playing video is a time jump.
const TIME_JUMP: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DriftAction {
    None,
    SetSpeed(f64),
    RestoreSpeed(f64),
    Seek(Duration),
    /// The clock jumped, so the anchor is gone: ask where the group is now.
    Resync,
}

#[derive(Debug, Default)]
pub struct DriftCorrector {
    anchor: Option<(Duration, Instant)>,
    /// When the active speed correction ends, and the speed to restore.
    correcting: Option<(Instant, f64)>,
    cooldown_until: Option<Instant>,
    observed_at: Option<Instant>,
}

impl DriftCorrector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resets all state and sets (or clears) the anchor; restores speed first
    /// if a correction was running.
    pub fn anchor(&mut self, position: Option<Duration>, now: Instant) -> DriftAction {
        let action = self.correcting.map_or(DriftAction::None, |(_, base)| {
            DriftAction::RestoreSpeed(base)
        });
        *self = Self {
            anchor: position.map(|p| (p, now)),
            ..Self::default()
        };
        action
    }

    pub fn observe(&mut self, position: Duration, speed: f64, now: Instant) -> DriftAction {
        let Some((anchor_pos, anchor_at)) = self.anchor else {
            return DriftAction::None;
        };
        // Anchor plus wall clock means nothing across a sleep.
        if self
            .observed_at
            .replace(now)
            .is_some_and(|last| now.saturating_duration_since(last) > TIME_JUMP)
        {
            self.anchor = None;
            return DriftAction::Resync;
        }
        if let Some((end, base)) = self.correcting {
            if now < end {
                return DriftAction::None;
            }
            self.correcting = None;
            self.cooldown_until = Some(now + COOLDOWN_AFTER_SPEED);
            return DriftAction::RestoreSpeed(base);
        }
        if self.cooldown_until.is_some_and(|c| now < c) || (speed - 1.0).abs() > 1e-6 {
            return DriftAction::None;
        }
        let expected = anchor_pos + now.saturating_duration_since(anchor_at);
        let behind = expected >= position;
        let drift = if behind {
            expected.saturating_sub(position)
        } else {
            position.saturating_sub(expected)
        };
        if drift < IGNORE_BELOW {
            return DriftAction::None;
        }
        if drift > SEEK_ABOVE {
            self.cooldown_until = Some(now + COOLDOWN_AFTER_SEEK);
            return DriftAction::Seek(expected);
        }
        let length = Duration::from_secs_f64(drift.as_secs_f64() / RATE_DELTA).min(MAX_CORRECTION);
        self.correcting = Some((now + length, speed));
        DriftAction::SetSpeed(if behind {
            1.0 + RATE_DELTA
        } else {
            1.0 - RATE_DELTA
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Anchored at 100 s at `t0`; returns the corrector and `t0`.
    fn anchored() -> (DriftCorrector, Instant) {
        let t0 = Instant::now();
        let mut d = DriftCorrector::new();
        assert_eq!(
            d.anchor(Some(Duration::from_secs(100)), t0),
            DriftAction::None
        );
        (d, t0)
    }

    fn is_speed(a: DriftAction, want: f64) -> bool {
        matches!(a, DriftAction::SetSpeed(s) if (s - want).abs() < 1e-9)
    }

    /// Observations once a second from `from` up to `to`: playing, mpv's
    /// `time-pos` is steady, so only a real time jump leaves a gap.
    fn keep_observing(
        d: &mut DriftCorrector,
        position: Duration,
        speed: f64,
        from: Instant,
        to: Instant,
    ) {
        let mut t = from + Duration::from_secs(1);
        while t < to {
            d.observe(position, speed, t);
            t += Duration::from_secs(1);
        }
    }

    #[test]
    fn small_drift_is_left_alone() {
        let (mut d, t0) = anchored();
        // expected 101 s, actual 100.9 s: 100 ms behind.
        let a = d.observe(ms(100_900), 1.0, t0 + Duration::from_secs(1));
        assert_eq!(a, DriftAction::None);
    }

    #[test]
    fn behind_speeds_up_ahead_slows_down() {
        let (mut d, t0) = anchored();
        let now = t0 + Duration::from_secs(1);
        assert!(is_speed(d.observe(ms(100_500), 1.0, now), 1.05));
        let (mut d, t0) = anchored();
        let now = t0 + Duration::from_secs(1);
        assert!(is_speed(d.observe(ms(101_500), 1.0, now), 0.95));
    }

    #[test]
    fn correction_lasts_drift_over_rate_then_restores() {
        let (mut d, t0) = anchored();
        let now = t0 + Duration::from_secs(1);
        // 500 ms drift / 0.05 = 10 s.
        assert!(is_speed(d.observe(ms(100_500), 1.0, now), 1.05));
        keep_observing(&mut d, ms(100_600), 1.05, now, now + ms(9_999));
        assert_eq!(
            d.observe(ms(100_600), 1.05, now + ms(9_999)),
            DriftAction::None
        );
        assert_eq!(
            d.observe(ms(110_000), 1.05, now + Duration::from_secs(10)),
            DriftAction::RestoreSpeed(1.0)
        );
        // 3 s cooldown after restoring: a fresh drift is ignored, then allowed.
        let later = now + Duration::from_secs(10);
        assert_eq!(
            d.observe(ms(100_000), 1.0, later + ms(2_999)),
            DriftAction::None
        );
        assert!(matches!(
            d.observe(ms(100_000), 1.0, later + Duration::from_secs(3)),
            DriftAction::Seek(_) | DriftAction::SetSpeed(_)
        ));
    }

    #[test]
    fn correction_duration_is_capped() {
        let (mut d, t0) = anchored();
        let now = t0 + Duration::from_secs(1);
        // 1.9 s drift would be 38 s uncapped.
        assert!(is_speed(d.observe(ms(99_100), 1.0, now), 1.05));
        keep_observing(&mut d, ms(99_200), 1.05, now, now + ms(29_999));
        assert_eq!(
            d.observe(ms(99_200), 1.05, now + ms(29_999)),
            DriftAction::None
        );
        assert_eq!(
            d.observe(ms(99_200), 1.05, now + Duration::from_secs(30)),
            DriftAction::RestoreSpeed(1.0)
        );
    }

    #[test]
    fn large_drift_seeks() {
        let (mut d, t0) = anchored();
        let now = t0 + Duration::from_secs(1);
        assert_eq!(
            d.observe(ms(90_000), 1.0, now),
            DriftAction::Seek(ms(101_000))
        );
        // 5 s cooldown.
        keep_observing(&mut d, ms(90_000), 1.0, now, now + ms(4_999));
        assert_eq!(
            d.observe(ms(90_000), 1.0, now + ms(4_999)),
            DriftAction::None
        );
        assert!(matches!(
            d.observe(ms(90_000), 1.0, now + Duration::from_secs(5)),
            DriftAction::Seek(_)
        ));
    }

    #[test]
    fn cooldown_blocks_a_repeat() {
        let (mut d, t0) = anchored();
        let now = t0 + Duration::from_secs(1);
        assert!(matches!(
            d.observe(ms(90_000), 1.0, now),
            DriftAction::Seek(_)
        ));
        assert_eq!(d.observe(ms(90_000), 1.0, now + ms(1)), DriftAction::None);
    }

    #[test]
    fn user_speed_is_left_alone() {
        let (mut d, t0) = anchored();
        let now = t0 + Duration::from_secs(1);
        assert_eq!(d.observe(ms(100_500), 1.5, now), DriftAction::None);
        assert_eq!(d.observe(ms(90_000), 1.5, now), DriftAction::None);
    }

    #[test]
    fn a_time_jump_asks_for_a_resync_and_never_seeks() {
        let (mut d, t0) = anchored();
        assert_eq!(d.observe(ms(100_000), 1.0, t0), DriftAction::None);
        // Woken after 4 s: the wall clock says 104 s, and mpv is still at 100 s.
        let woken = t0 + Duration::from_secs(4);
        assert_eq!(d.observe(ms(100_000), 1.0, woken), DriftAction::Resync);
        assert_eq!(
            d.observe(ms(100_000), 1.0, woken + ms(1)),
            DriftAction::None,
            "the anchor is gone"
        );
    }

    #[test]
    fn a_three_second_gap_is_not_a_time_jump() {
        let (mut d, t0) = anchored();
        assert_eq!(d.observe(ms(100_000), 1.0, t0), DriftAction::None);
        assert_eq!(
            d.observe(ms(100_000), 1.0, t0 + Duration::from_secs(3)),
            DriftAction::Seek(ms(103_000))
        );
    }

    #[test]
    fn a_speed_correction_is_still_undone_after_a_time_jump() {
        let (mut d, t0) = anchored();
        let now = t0 + Duration::from_secs(1);
        assert!(is_speed(d.observe(ms(100_500), 1.0, now), 1.05));
        let woken = now + Duration::from_secs(4);
        assert_eq!(d.observe(ms(100_600), 1.05, woken), DriftAction::Resync);
        assert_eq!(d.anchor(None, woken), DriftAction::RestoreSpeed(1.0));
    }

    #[test]
    fn anchor_none_restores_speed() {
        let (mut d, t0) = anchored();
        let now = t0 + Duration::from_secs(1);
        assert!(is_speed(d.observe(ms(100_500), 1.0, now), 1.05));
        assert_eq!(d.anchor(None, now + ms(1)), DriftAction::RestoreSpeed(1.0));
        // No anchor: nothing happens; a second clear is a no-op.
        assert_eq!(d.observe(ms(1), 1.0, now + ms(2)), DriftAction::None);
        assert_eq!(d.anchor(None, now + ms(3)), DriftAction::None);
    }
}
