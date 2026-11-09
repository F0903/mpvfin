//! An item's titles and picture.

use serde_json::Value;

use crate::client::{Client, HttpError};
use crate::identity::pct;

/// Human-readable title for an item.
#[must_use]
pub fn display_title(item: &Value) -> String {
    let s = |k: &str| item.get(k).and_then(Value::as_str);
    let n = |k: &str| item.get(k).and_then(Value::as_i64);
    let name = s("Name").unwrap_or_default();
    if let (Some(series), Some(season), Some(ep)) =
        (s("SeriesName"), n("ParentIndexNumber"), n("IndexNumber"))
    {
        return format!("{series} S{season:02}E{ep:02} - {name}");
    }
    match n("ProductionYear") {
        Some(y) => format!("{name} ({y})"),
        None => name.to_owned(),
    }
}

/// The title without the series, for where space is tight.
#[must_use]
pub fn short_title(item: &Value) -> String {
    let s = |k: &str| item.get(k).and_then(Value::as_str);
    let n = |k: &str| item.get(k).and_then(Value::as_i64);
    match (s("SeriesName"), n("ParentIndexNumber"), n("IndexNumber")) {
        (Some(_), Some(season), Some(ep)) => {
            format!("S{season:02}E{ep:02} · {}", s("Name").unwrap_or_default())
        }
        _ => display_title(item),
    }
}

/// The most of a reply to read; the request asks for far less.
pub const MAX_THUMBNAIL_BYTES: u64 = 4 * 1024 * 1024;

/// `GET` path for an item's picture, small enough to overlay and still sharp on 4K.
#[must_use]
pub fn thumbnail_path(item_id: &str) -> String {
    format!(
        "/Items/{}/Images/Primary?maxWidth=640&maxHeight=360&format=Jpg&quality=90",
        pct(item_id)
    )
}

impl Client {
    /// An item's title for where space is tight; see [`short_title`].
    ///
    /// # Errors
    /// The request failed.
    pub fn short_title(&self, user_id: &str, item_id: &str) -> Result<String, HttpError> {
        let item = self.get(&format!("/Items/{}?userId={}", pct(item_id), pct(user_id)))?;
        Ok(short_title(&item))
    }

    /// An item's picture as a JPEG, small enough to overlay and still sharp on 4K.
    ///
    /// # Errors
    /// The request failed, the item has no picture, or the reply is too long.
    pub fn thumbnail(&self, item_id: &str) -> Result<Vec<u8>, HttpError> {
        self.get_bytes(&thumbnail_path(item_id), MAX_THUMBNAIL_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn title_formats_episodes_movies_and_fallback() {
        let ep = json!({"Name": "Pilot", "SeriesName": "Test Show", "ParentIndexNumber": 1, "IndexNumber": 2});
        assert_eq!(display_title(&ep), "Test Show S01E02 - Pilot");
        assert_eq!(
            display_title(&json!({"Name": "X", "ProductionYear": 2020})),
            "X (2020)"
        );
        assert_eq!(display_title(&json!({"Name": "Name"})), "Name");
    }
    #[test]
    fn short_title_drops_the_series_from_episodes() {
        let ep = json!({"Name": "Pilot", "SeriesName": "Test Show", "ParentIndexNumber": 1, "IndexNumber": 2});
        assert_eq!(short_title(&ep), "S01E02 · Pilot");
        assert_eq!(
            short_title(&json!({"Name": "X", "ProductionYear": 2020})),
            "X (2020)"
        );
        assert_eq!(short_title(&json!({})), "");
    }
    #[test]
    fn the_path_escapes_the_id_and_asks_for_a_small_jpeg() {
        assert_eq!(
            thumbnail_path("a b/c"),
            "/Items/a%20b%2Fc/Images/Primary?maxWidth=640&maxHeight=360&format=Jpg&quality=90"
        );
    }
}
