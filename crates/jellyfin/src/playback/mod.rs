//! Playing an item: negotiating its media, what this client can play,
//! reports, segments, and the item's titles and picture.

mod items;
mod media;
mod profile;
mod report;
mod segments;

pub use media::{PlayMethod, ResolvedMedia, TrackMap};
pub use report::PlaybackProgress;
pub use segments::{Segment, SegmentKind};
