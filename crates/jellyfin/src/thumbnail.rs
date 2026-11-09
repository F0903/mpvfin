//! Where an item's picture is, and how much of it to read.

use crate::client::{Client, HttpError};
use crate::identity::pct;

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

    #[test]
    fn the_path_escapes_the_id_and_asks_for_a_small_jpeg() {
        assert_eq!(
            thumbnail_path("a b/c"),
            "/Items/a%20b%2Fc/Images/Primary?maxWidth=640&maxHeight=360&format=Jpg&quality=90"
        );
    }
}
