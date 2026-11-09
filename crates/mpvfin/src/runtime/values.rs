//! Reading mpv's property values.

use std::time::Duration;

use crate::runtime::Observed;

/// A `time-pos` value; `None` when unavailable.
pub(super) fn parse_position(value: Option<&str>) -> Option<Duration> {
    value
        .and_then(|v| v.parse::<f64>().ok())
        .and_then(|secs| Duration::try_from_secs_f64(secs.max(0.0)).ok())
}

pub(super) fn yes_no(flag: bool) -> &'static str {
    if flag { "yes" } else { "no" }
}

/// An `osd-width` or `osd-height` value; 0 when missing or unparsable.
pub(super) fn osd_length(value: Option<&str>) -> i64 {
    value.and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// The OSD size to lay the button out on: without a video output mpv
/// reports none, and the hit test must still work.
pub(super) fn osd_size(observed: &Observed) -> (i64, i64) {
    match (observed.osd_width, observed.osd_height) {
        (w, h) if w > 0 && h > 0 => (w, h),
        _ => (1280, 720),
    }
}

/// mpv's volume (up to 130) as a Jellyfin percentage.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=100 first
pub(super) fn percent(volume: f64) -> u8 {
    volume.clamp(0.0, 100.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_zero_osd_counts_as_720p() {
        let size = |osd_width, osd_height| {
            osd_size(&Observed {
                osd_width,
                osd_height,
                ..Observed::default()
            })
        };
        assert_eq!(size(1920, 1080), (1920, 1080));
        assert_eq!(size(0, 0), (1280, 720));
        assert_eq!(size(1920, 0), (1280, 720));
        assert_eq!(osd_length(None), 0);
        assert_eq!(osd_length(Some("1920")), 1920);
    }
    #[test]
    fn volume_is_clamped_and_rounded() {
        assert_eq!(percent(130.0), 100);
        assert_eq!(percent(-1.0), 0);
        assert_eq!(percent(49.6), 50);
    }
}
