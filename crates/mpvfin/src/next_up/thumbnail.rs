//! Decoding an item's picture into what mpv overlays.

use jpeg_decoder::{Decoder, PixelFormat};

/// Largest side accepted, so a bad reply can't claim gigabytes of pixels.
const MAX_SIDE: u16 = 1920;

/// A decoded picture.
#[derive(Debug)]
pub struct Thumbnail {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Opaque BGRA, row after row with no padding.
    pub bgra: Vec<u8>,
}

/// Decodes a JPEG; `None` for anything else or an implausibly large one.
#[must_use]
pub fn decode_thumbnail(jpeg: &[u8]) -> Option<Thumbnail> {
    let mut decoder = Decoder::new(jpeg);
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    if info.pixel_format != PixelFormat::RGB24 || info.width > MAX_SIDE || info.height > MAX_SIDE {
        return None;
    }
    let rgb = decoder.decode().ok()?;
    Some(Thumbnail {
        width: u32::from(info.width),
        height: u32::from(info.height),
        bgra: rgb
            .chunks_exact(3)
            .flat_map(|p| [p[2], p[1], p[0], 255])
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16x8, solid red.
    const JPEG: &[u8] = include_bytes!("../../tests/fixtures/thumb.jpg");

    #[test]
    fn a_jpeg_decodes_to_opaque_bgra() {
        let t = decode_thumbnail(JPEG).expect("a valid JPEG");
        assert_eq!((t.width, t.height), (16, 8));
        assert_eq!(t.bgra.len(), 16 * 8 * 4);
        let pixel = &t.bgra[..4];
        assert!(
            pixel[2] > 200 && pixel[1] < 40 && pixel[0] < 40,
            "not red in BGRA order: {pixel:?}"
        );
        assert_eq!(pixel[3], 255, "opaque");
    }

    #[test]
    fn anything_else_is_refused() {
        assert!(decode_thumbnail(b"{\"not\":\"an image\"}").is_none());
        assert!(decode_thumbnail(&[]).is_none());
        assert!(decode_thumbnail(&JPEG[..JPEG.len() / 2]).is_none());
    }

    #[test]
    fn an_oversized_jpeg_is_refused_before_decoding() {
        let mut huge = JPEG.to_vec();
        let frame = huge
            .windows(2)
            .position(|w| w == [0xFF, 0xC0])
            .expect("a baseline frame header");
        // The header's width field: claims far more pixels than the data holds.
        huge[frame + 7..frame + 9].copy_from_slice(&30_000u16.to_be_bytes());
        assert!(decode_thumbnail(&huge).is_none());
    }
}
