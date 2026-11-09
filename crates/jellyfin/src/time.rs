//! Jellyfin's time formats: 100 ns ticks, and UTC timestamps.

use std::time::Duration;

/// Converts 100-nanosecond ticks to a duration; negative ticks give zero.
#[must_use]
pub fn ticks_to_duration(ticks: i64) -> Duration {
    u64::try_from(ticks).map_or(Duration::ZERO, |t| {
        Duration::from_nanos(t.saturating_mul(100))
    })
}

/// Converts a duration to 100-nanosecond ticks, saturating.
#[must_use]
pub fn duration_to_ticks(d: Duration) -> i64 {
    i64::try_from(d.as_nanos() / 100).unwrap_or(i64::MAX)
}

pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.f...]Z` to ms since the epoch; extra
/// fractional digits are truncated. Never panics.
#[must_use]
pub fn parse_utc_ms(text: &str) -> Option<i64> {
    let b = text.strip_suffix('Z')?.as_bytes();
    if b.len() < 19 || !b.is_ascii() {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let digits = &b[r];
        if digits.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(digits).ok()?.parse().ok()
        } else {
            None
        }
    };
    if [b[4], b[7], b[10], b[13], b[16]] != *b"--T::" {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let frac = match &b[19..] {
        [] => 0,
        [b'.', digits @ ..] if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) => {
            let mut ms = 0;
            for i in 0..3 {
                ms = ms * 10 + i64::from(digits.get(i).map_or(0, |c| c - b'0'));
            }
            ms
        }
        _ => return None,
    };
    let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec;
    Some(secs * 1000 + frac)
}

/// Formats ms since the epoch as UTC with millisecond precision and `Z`.
#[must_use]
pub fn format_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60,
        ms.rem_euclid(1000)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_convert_both_ways() {
        assert_eq!(ticks_to_duration(-5), Duration::ZERO);
        assert_eq!(ticks_to_duration(300_000_000), Duration::from_secs(30));
        assert_eq!(duration_to_ticks(Duration::from_secs(30)), 300_000_000);
        assert_eq!(ticks_to_duration(i64::MAX).as_secs(), 18_446_744_073);
    }
    #[test]
    fn parse_utc_handles_variable_fractions() {
        let base = 1_767_323_045_000; // 2026-01-02T03:04:05Z
        assert_eq!(parse_utc_ms("2026-01-02T03:04:05Z"), Some(base));
        assert_eq!(parse_utc_ms("2026-01-02T03:04:05.5Z"), Some(base + 500));
        assert_eq!(parse_utc_ms("2026-01-02T03:04:05.957Z"), Some(base + 957));
        assert_eq!(
            parse_utc_ms("2026-01-02T03:04:05.941608Z"),
            Some(base + 941)
        );
        assert_eq!(
            parse_utc_ms("2026-01-02T03:04:05.3987244Z"),
            Some(base + 398)
        );
        assert_eq!(parse_utc_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc_ms("2000-02-29T00:00:00Z"), Some(951_782_400_000));
    }
    #[test]
    fn parse_utc_rejects_garbage() {
        for s in [
            "",
            "Z",
            "garbage",
            "2026-01-02T03:04:05",
            "2026-01-02 03:04:05Z",
            "2026-13-02T03:04:05Z",
            "2026-01-02T25:04:05Z",
            "2026-01-02T03:04:05.Z",
            "2026-01-02T03:04:05.1xZ",
            "2026-01-02T03:04:05+01:00Z",
            "20\u{e9}6-01-02T03:04:05Z",
            "+026-01-02T03:04:05Z",
            "2026-01-02T03:04:05.5ZZ",
        ] {
            assert_eq!(parse_utc_ms(s), None, "{s:?}");
        }
    }
    #[test]
    fn format_utc_round_trips() {
        assert_eq!(format_utc(1_767_323_045_007), "2026-01-02T03:04:05.007Z");
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        for ms in [
            0,
            1,
            951_782_400_123,
            1_767_323_045_999,
            4_102_444_800_000,
            86_400_000,
        ] {
            assert_eq!(parse_utc_ms(&format_utc(ms)), Some(ms), "{ms}");
        }
    }
}
