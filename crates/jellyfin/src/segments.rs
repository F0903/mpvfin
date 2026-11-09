//! Media segment (intro, recap, outro, preview) request path and parsing.

use std::time::Duration;

use serde_json::Value;

use crate::client::{Client, HttpError};
use crate::identity::pct;
use crate::wire::ticks_to_duration;

/// What a media segment is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentKind {
    /// Opening credits.
    Intro,
    /// Recap of earlier episodes.
    Recap,
    /// Closing credits.
    Outro,
    /// Preview of the next episode.
    Preview,
}

/// A skippable span of the playing item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    /// What the segment is.
    pub kind: SegmentKind,
    /// Where it starts.
    pub start: Duration,
    /// Where it ends.
    pub end: Duration,
    /// Skipping seeks to the end of the file instead of to `end`.
    pub to_end_of_file: bool,
}

/// A segment ending this close to the end of the file skips to the file end.
pub const END_OF_FILE_TOLERANCE: Duration = Duration::from_secs(5);

/// `GET` path for an item's segments of the four skippable types.
#[must_use]
pub fn segments_path(item_id: &str) -> String {
    format!(
        "/MediaSegments/{}?includeSegmentTypes=Intro&includeSegmentTypes=Recap&includeSegmentTypes=Outro&includeSegmentTypes=Preview",
        pct(item_id)
    )
}

/// Segments from a `MediaSegments` response, sorted by start; bad entries are dropped.
#[must_use]
pub fn parse_segments(response: &Value, runtime_ticks: Option<i64>) -> Vec<Segment> {
    let runtime = runtime_ticks.map(ticks_to_duration);
    let items = response.get("Items").and_then(Value::as_array);
    let mut segments: Vec<Segment> = items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let kind = match item.get("Type")?.as_str()? {
                "Intro" => SegmentKind::Intro,
                "Recap" => SegmentKind::Recap,
                "Outro" => SegmentKind::Outro,
                "Preview" => SegmentKind::Preview,
                _ => return None,
            };
            let start = ticks_to_duration(item.get("StartTicks")?.as_i64()?);
            let end = ticks_to_duration(item.get("EndTicks")?.as_i64()?);
            (end > start).then(|| Segment {
                kind,
                start,
                end,
                to_end_of_file: runtime.is_some_and(|r| end + END_OF_FILE_TOLERANCE >= r),
            })
        })
        .collect();
    segments.sort_by_key(|s| s.start);
    segments
}

impl Client {
    /// An item's skippable segments, sorted by start. `runtime_ticks` decides
    /// which of them run to the end of the file.
    ///
    /// # Errors
    /// The request failed.
    pub fn segments(
        &self,
        item_id: &str,
        runtime_ticks: Option<i64>,
    ) -> Result<Vec<Segment>, HttpError> {
        Ok(parse_segments(
            &self.get(&segments_path(item_id))?,
            runtime_ticks,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const S: i64 = 10_000_000;

    #[test]
    fn segments_path_requests_only_the_four_types() {
        assert_eq!(
            segments_path("a b/c"),
            "/MediaSegments/a%20b%2Fc?includeSegmentTypes=Intro&includeSegmentTypes=Recap&includeSegmentTypes=Outro&includeSegmentTypes=Preview"
        );
    }

    #[test]
    fn parse_segments_maps_ticks_and_kinds() {
        let r = json!({"Items": [
            {"Type": "Outro", "StartTicks": 100 * S, "EndTicks": 120 * S},
            {"Type": "Intro", "StartTicks": 5 * S, "EndTicks": 20 * S},
        ]});
        assert_eq!(
            parse_segments(&r, Some(200 * S)),
            vec![
                Segment {
                    kind: SegmentKind::Intro,
                    start: Duration::from_secs(5),
                    end: Duration::from_secs(20),
                    to_end_of_file: false,
                },
                Segment {
                    kind: SegmentKind::Outro,
                    start: Duration::from_secs(100),
                    end: Duration::from_mins(2),
                    to_end_of_file: false,
                },
            ]
        );
    }

    #[test]
    fn parse_segments_drops_bad_and_unwanted_entries() {
        let r = json!({"Items": [
            {"Type": "Commercial", "StartTicks": 0, "EndTicks": S},
            {"Type": "Unknown", "StartTicks": 0, "EndTicks": S},
            {"Type": "Intro", "StartTicks": 5 * S, "EndTicks": 5 * S},
            {"Type": "Intro", "StartTicks": 6 * S, "EndTicks": 5 * S},
            {"Type": "Intro", "EndTicks": S},
            {"Type": "Intro", "StartTicks": 0},
            "text",
            7,
            {"Type": "Recap", "StartTicks": 0, "EndTicks": S},
        ]});
        let got = parse_segments(&r, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, SegmentKind::Recap);
        assert!(parse_segments(&json!({}), None).is_empty());
    }

    #[test]
    fn to_end_of_file_uses_the_tolerance() {
        let at = |end: i64, runtime: Option<i64>| {
            let r = json!({"Items": [{"Type": "Outro", "StartTicks": 0, "EndTicks": end * S}]});
            parse_segments(&r, runtime.map(|s| s * S))[0].to_end_of_file
        };
        assert!(at(115, Some(120)));
        assert!(!at(114, Some(120)));
        assert!(!at(120, None));
    }
}
