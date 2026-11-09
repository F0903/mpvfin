//! Pure geometry, drawing and input commands of the Next Up card.

use crate::skip_button::{
    Rect, button_events, button_rect, clear_overlay_command, input_section_commands,
    overlay_command, rounded_rect_path,
};

/// Input section holding Play Now's keyboard shortcut, enabled while the card is up.
pub const KEY_SECTION: &str = "mpvfin-next-up-key";
/// Input section holding Play Now's click, enabled while the pointer is over it.
pub const CLICK_SECTION: &str = "mpvfin-next-up-click";

const OVERLAY_ID: &str = "2";

/// `overlay-add` ids are shared by every script in mpv; this one is out of the usual way.
const THUMBNAIL_ID: &str = "57";

/// Longest label drawn whole. A rough fit: the card clips what this misses.
const MAX_LABEL_CHARS: usize = 26;

/// Where the card's parts sit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    /// The card itself: above the skip button, flush right.
    pub card: Rect,
    /// The Play Now button.
    pub play: Rect,
    /// The picture, when there is one.
    pub thumbnail: Option<Rect>,
}

/// Lays the card out around its content on an OSD of this size; `thumbnail`
/// is the picture's size in pixels.
#[must_use]
pub fn layout(label: &str, thumbnail: Option<(u32, u32)>, osd_w: i64, osd_h: i64) -> Layout {
    let skip = button_rect(osd_w, osd_h);
    let unit = skip.h;
    let pad = unit / 3;
    let inner_h = unit * 9 / 10 + pad + unit;
    let picture = thumbnail.map(|size| fit(size, inner_h * 16 / 9, inner_h));
    let text_w = text_width(label, unit);
    let w = pad + text_w + pad + picture.map_or(0, |(w, _)| w + pad);
    let h = pad + inner_h + pad;
    let card = Rect {
        x: skip.x + skip.w - w,
        y: skip.y - unit / 4 - h,
        w,
        h,
    };
    Layout {
        card,
        play: Rect {
            x: card.x + pad,
            y: card.y + card.h - pad - unit,
            w: text_w,
            h: unit,
        },
        thumbnail: picture.map(|(w, h)| Rect {
            x: card.x + card.w - pad - w,
            y: card.y + pad + (inner_h - h) / 2,
            w,
            h,
        }),
    }
}

/// The largest size of `size`'s shape inside `box_w` by `box_h`.
fn fit(size: (u32, u32), box_w: i64, box_h: i64) -> (i64, i64) {
    let (w, h) = (i64::from(size.0).max(1), i64::from(size.1).max(1));
    if w * box_h > h * box_w {
        (box_w, h * box_w / w)
    } else {
        (w * box_h / h, box_h)
    }
}

/// `overlay-add` command showing BGRA pixels at `at`, scaled to fit it.
///
/// mpv is handed the pixels' address: they must outlive the overlay.
#[must_use]
pub fn thumbnail_command(bgra: &[u8], size: (u32, u32), at: Rect) -> Vec<String> {
    let address = format!("&{}", bgra.as_ptr().expose_provenance());
    let (w, h) = size;
    [
        "overlay-add",
        THUMBNAIL_ID,
        &at.x.to_string(),
        &at.y.to_string(),
        &address,
        "0",
        "bgra",
        &w.to_string(),
        &h.to_string(),
        &(w * 4).to_string(),
        &at.w.to_string(),
        &at.h.to_string(),
    ]
    .map(str::to_owned)
    .into()
}

/// `overlay-remove` command for the picture.
#[must_use]
pub fn remove_thumbnail_command() -> Vec<String> {
    ["overlay-remove", THUMBNAIL_ID].map(str::to_owned).into()
}

/// `label` cut to [`MAX_LABEL_CHARS`], then made safe to put in an ASS event.
fn label_text(label: &str) -> String {
    let mut chars = label.chars();
    let mut kept: String = chars.by_ref().take(MAX_LABEL_CHARS).collect();
    if chars.next().is_some() {
        kept.pop();
        kept.push('…');
    }
    let mut escaped = String::with_capacity(kept.len());
    for c in kept.chars() {
        match c {
            // The zero-width no-break space stops it starting an ASS escape.
            '\\' => escaped.push_str("\\\u{feff}"),
            '{' => escaped.push_str("\\{"),
            '}' => escaped.push_str("\\}"),
            c if c.is_control() => escaped.push(' '),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Room for the label beside the picture: estimated, as there are no font metrics here.
fn text_width(label: &str, unit: i64) -> i64 {
    let chars = i64::try_from(label.chars().count().min(MAX_LABEL_CHARS)).unwrap_or(0);
    (chars * unit * 21 / 100).max(unit * 4)
}

/// `osd-overlay` command drawing the card.
#[must_use]
pub fn draw_command(
    label: &str,
    thumbnail: Option<(u32, u32)>,
    osd_w: i64,
    osd_h: i64,
    hovered: bool,
) -> Vec<String> {
    let Layout { card, play, .. } = layout(label, thumbnail, osd_w, osd_h);
    let Rect { x, y, w, h } = card;
    let unit = play.h;
    let pad = unit / 3;
    let (text_x, heading_y, label_y) = (x + pad, y + pad, y + pad + unit * 4 / 10);
    let (right, bottom) = (text_x + text_width(label, unit) + pad / 2, y + h);
    let body = rounded_rect_path(w, h, unit / 4);
    let background =
        format!(r"{{\an7\pos({x},{y})\bord0\shad0\1c&H141414&\1a&H20&\p1}}{body}{{\p0}}");
    let heading = format!(
        r"{{\an7\pos({text_x},{heading_y})\bord0\shad0\1c&HB0B0B0&\1a&H00&\fs{}}}Next Up",
        unit * 35 / 100
    );
    let title = format!(
        r"{{\an7\pos({text_x},{label_y})\clip({text_x},{y},{right},{bottom})\q2\bord0\shad0\1c&HFFFFFF&\1a&H00&\b1\fs{}}}{}",
        unit * 42 / 100,
        label_text(label)
    );
    let play = button_events(play, "Play Now", hovered);
    let data = format!("{background}\n{heading}\n{title}\n{play}");
    overlay_command(OVERLAY_ID, &data, osd_w, osd_h)
}

/// `osd-overlay` command removing the card.
#[must_use]
pub fn clear_command() -> Vec<String> {
    clear_overlay_command(OVERLAY_ID)
}

/// Commands defining the card's input sections, inert until enabled.
#[must_use]
pub fn define_section_commands(client: &str) -> Vec<Vec<String>> {
    input_section_commands(KEY_SECTION, CLICK_SECTION, "n", client, "next-up")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skip_button::{button_rect, set_section_command};

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn rect(x: i64, y: i64, w: i64, h: i64) -> Rect {
        Rect { x, y, w, h }
    }

    const LABEL: &str = "S01E02 · The Kingsroad";

    fn drawn(label: &str) -> String {
        draw_command(label, None, 1280, 720, false).swap_remove(3)
    }

    #[test]
    fn a_card_without_a_picture_ends_just_past_its_label() {
        assert_eq!(
            layout(LABEL, None, 1280, 720),
            Layout {
                card: rect(1011, 444, 226, 123),
                play: rect(1025, 510, 198, 43),
                thumbnail: None,
            }
        );
    }

    #[test]
    fn a_picture_sits_right_of_the_text_and_fills_the_cards_height() {
        assert_eq!(
            layout(LABEL, Some((640, 360)), 1280, 720),
            Layout {
                card: rect(829, 444, 408, 123),
                play: rect(843, 510, 198, 43),
                thumbnail: Some(rect(1055, 458, 168, 94)),
            }
        );
    }

    #[test]
    fn a_portrait_picture_narrows_the_card() {
        let Layout {
            card, thumbnail, ..
        } = layout(LABEL, Some((400, 600)), 1280, 720);
        assert_eq!(card.w, 303);
        assert_eq!(thumbnail, Some(rect(1160, 458, 63, 95)));
    }

    #[test]
    fn a_wide_picture_is_centred_vertically() {
        let thumbnail = layout(LABEL, Some((1000, 400)), 1280, 720).thumbnail;
        assert_eq!(thumbnail, Some(rect(1055, 472, 168, 67)));
    }

    #[test]
    fn the_text_is_at_least_the_button_wide_and_at_most_the_longest_label() {
        assert_eq!(layout("x", None, 1280, 720).card.w, 14 + 172 + 14);
        let long = "é".repeat(60);
        assert_eq!(layout(&long, None, 1280, 720).card.w, 14 + 234 + 14);
    }

    #[test]
    fn play_now_fills_the_space_up_to_the_picture_or_the_cards_edge() {
        let pad = 14;
        let with = layout(LABEL, Some((640, 360)), 1280, 720);
        let picture = with.thumbnail.expect("a picture");
        assert_eq!(with.play.x + with.play.w + pad, picture.x);
        let without = layout(LABEL, None, 1280, 720);
        assert_eq!(
            without.play.x + without.play.w + pad,
            without.card.x + without.card.w
        );
    }

    #[test]
    fn the_card_stays_flush_right_above_the_skip_button() {
        for (w, h) in [(1280, 720), (1920, 1080), (640, 360)] {
            for picture in [None, Some((640, 360))] {
                let Layout { card, play, .. } = layout(LABEL, picture, w, h);
                let skip = button_rect(w, h);
                assert_eq!(card.x + card.w, skip.x + skip.w, "{w}x{h}");
                assert!(card.y + card.h <= skip.y, "overlaps the button at {w}x{h}");
                assert_eq!(play.h, skip.h, "{w}x{h}");
                assert!(play.w >= skip.w, "narrower than Skip at {w}x{h}");
                assert!(card.contains(play.x, play.y), "{w}x{h}");
                assert!(
                    card.contains(play.x + play.w - 1, play.y + play.h - 1),
                    "{w}x{h}"
                );
            }
        }
    }

    #[test]
    fn the_picture_is_handed_over_by_address_and_scaled_to_its_place() {
        let pixels = vec![0u8; 16 * 8 * 4];
        let address = format!("&{}", pixels.as_ptr().expose_provenance());
        assert_eq!(
            thumbnail_command(&pixels, (16, 8), rect(10, 20, 32, 16)),
            cmd(&[
                "overlay-add",
                "57",
                "10",
                "20",
                &address,
                "0",
                "bgra",
                "16",
                "8",
                "64",
                "32",
                "16"
            ])
        );
        assert_eq!(remove_thumbnail_command(), cmd(&["overlay-remove", "57"]));
    }

    #[test]
    fn draw_command_is_its_own_overlay_and_says_what_is_next() {
        let command = draw_command("S01E02 · Pilot", None, 1280, 720, false);
        assert_eq!(command.len(), 9);
        assert_eq!(command[..3], cmd(&["osd-overlay", "2", "ass-events"]));
        assert_eq!(command[4..], cmd(&["1280", "720", "0", "no", "no"]));
        assert!(command[3].contains("}Next Up"), "{}", command[3]);
        assert!(command[3].contains("}S01E02 · Pilot"), "{}", command[3]);
        assert!(command[3].contains("}Play Now "), "{}", command[3]);
    }

    #[test]
    fn hover_changes_only_the_buttons_fill() {
        let idle = draw_command("x", None, 1280, 720, false);
        let hovered = draw_command("x", None, 1280, 720, true);
        let changed = idle[3]
            .split('\n')
            .zip(hovered[3].split('\n'))
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changed, 1);
    }

    #[test]
    fn a_title_cannot_inject_ass_or_break_the_event_list() {
        let text = drawn("a{\\b0}\\n\nb");
        assert_eq!(text.lines().count(), drawn("x").lines().count());
        assert!(text.contains(r"a\{\"), "{text}");
        assert!(text.contains("\\\u{feff}b0\\}"), "{text}");
        assert!(text.contains("\\\u{feff}n b"), "{text}");
    }

    #[test]
    fn a_long_label_is_shortened_on_a_character_boundary() {
        let long = "é".repeat(60);
        let text = drawn(&long);
        assert!(text.contains(&format!("}}{}…", "é".repeat(25))), "{text}");
        assert!(!text.contains(&"é".repeat(26)), "{text}");
        let exact = "é".repeat(26);
        assert!(drawn(&exact).contains(&exact), "26 characters fit");
    }

    #[test]
    fn the_label_is_clipped_before_the_picture() {
        assert!(
            drawn("x").contains(r"\clip(1051,444,1230,567)"),
            "{}",
            drawn("x")
        );
    }

    #[test]
    fn clear_command_removes_only_the_card() {
        assert_eq!(
            clear_command(),
            cmd(&["osd-overlay", "2", "none", "", "0", "720", "0", "no", "no"])
        );
    }

    #[test]
    fn sections_use_n_with_default_and_click_with_force() {
        assert_eq!(
            define_section_commands("mpvfin"),
            vec![
                cmd(&[
                    "define-section",
                    "mpvfin-next-up-key",
                    "n script-binding mpvfin/next-up\n",
                    "default"
                ]),
                cmd(&[
                    "define-section",
                    "mpvfin-next-up-click",
                    "MBTN_LEFT script-binding mpvfin/next-up\nMBTN_LEFT_DBL ignore\n",
                    "force"
                ]),
            ]
        );
    }

    #[test]
    fn the_key_section_leaves_the_cursor_and_window_alone() {
        assert_eq!(
            set_section_command(KEY_SECTION, true),
            cmd(&[
                "enable-section",
                "mpvfin-next-up-key",
                "allow-hide-cursor+allow-vo-dragging"
            ])
        );
        assert_eq!(
            set_section_command(CLICK_SECTION, true),
            cmd(&["enable-section", "mpvfin-next-up-click"])
        );
    }
}
