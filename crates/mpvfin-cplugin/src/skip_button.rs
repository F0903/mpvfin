//! Pure geometry, drawing and input commands of the skip button.

use mpvfin_core::SegmentKind;

/// Input section holding the button's keyboard shortcut, enabled while it is up.
pub const KEY_SECTION: &str = "mpvfin-skip-key";
/// Input section holding the button's click, enabled while the pointer is over it.
pub const CLICK_SECTION: &str = "mpvfin-skip-click";

/// A rectangle in OSD pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

impl Rect {
    /// Whether the point is inside; left and top edges included, right and
    /// bottom excluded.
    #[must_use]
    pub fn contains(&self, x: i64, y: i64) -> bool {
        (self.x..self.x + self.w).contains(&x) && (self.y..self.y + self.h).contains(&y)
    }
}

/// Where the button sits on an OSD of this size.
#[must_use]
pub fn button_rect(osd_w: i64, osd_h: i64) -> Rect {
    let h = (osd_h * 6 / 100).max(36);
    let w = h * 4;
    Rect {
        x: osd_w - w - h,
        y: osd_h - h - osd_h * 14 / 100,
        w,
        h,
    }
}

/// The button's text for a segment kind.
#[must_use]
pub const fn label(kind: SegmentKind) -> &'static str {
    match kind {
        SegmentKind::Intro => "Skip Intro",
        SegmentKind::Recap => "Skip Recap",
        SegmentKind::Outro => "Skip Credits",
        SegmentKind::Preview => "Skip Preview",
    }
}

/// ASS path of a rounded rectangle filling a `w` by `h` box.
fn rounded_rect_path(w: i64, h: i64) -> String {
    let r = h / 8;
    // Bezier handle length that makes a quarter circle.
    let c = r * 55 / 100;
    let (right, bottom) = (w - r, h - r);
    let (near, far_x, far_y) = (r - c, right + c, bottom + c);
    format!(
        "m {r} 0 l {right} 0 b {far_x} 0 {w} {near} {w} {r} l {w} {bottom} b {w} {far_y} {far_x} {h} {right} {h} l {r} {h} b {near} {h} 0 {far_y} 0 {bottom} l 0 {r} b 0 {near} {near} 0 {r} 0"
    )
}

/// ASS path of a skip-next symbol, a triangle and a bar, `size` tall.
fn skip_icon_path(size: i64) -> String {
    let (tip, left, right) = (size * 5 / 8, size * 3 / 4, size * 3 / 4 + size / 5);
    let middle = size / 2;
    format!(
        "m 0 0 l {tip} {middle} l 0 {size} m {left} 0 l {right} 0 l {right} {size} l {left} {size}"
    )
}

/// `osd-overlay` command drawing the button.
#[must_use]
pub fn draw_command(kind: SegmentKind, osd_w: i64, osd_h: i64, hovered: bool) -> Vec<String> {
    let Rect { x, y, w, h } = button_rect(osd_w, osd_h);
    let fill = if hovered { "505050" } else { "303030" };
    let body = rounded_rect_path(w, h);
    let (center_x, center_y, font_size, text) = (x + w / 2, y + h / 2, h * 45 / 100, label(kind));
    let icon = skip_icon_path(font_size * 3 / 4);
    let drop = font_size / 8;
    let background =
        format!(r"{{\an7\pos({x},{y})\bord0\shad0\1c&H{fill}&\1a&H00&\p1}}{body}{{\p0}}");
    // The icon is drawn inline, so libass centres the label and icon together.
    let content = format!(
        r"{{\an5\pos({center_x},{center_y})\bord0\shad0\1c&HE6E6E6&\1a&H00&\b1\fs{font_size}}}{text} {{\p1\pbo{drop}}}{icon}{{\p0}}"
    );
    let data = format!("{background}\n{content}");
    [
        "osd-overlay",
        "1",
        "ass-events",
        &data,
        &osd_w.to_string(),
        &osd_h.to_string(),
        "0",
        "no",
        "no",
    ]
    .map(str::to_owned)
    .into()
}

/// `osd-overlay` command removing the button.
#[must_use]
pub fn clear_command() -> Vec<String> {
    ["osd-overlay", "1", "none", "", "0", "720", "0", "no", "no"]
        .map(str::to_owned)
        .into()
}

/// Commands defining the button's input sections, inert until enabled.
///
/// `default` lets a user's own binding of the key win; `force` makes the click
/// win over anything else, and is only enabled while the pointer is on the button.
#[must_use]
pub fn define_section_commands(client: &str) -> Vec<Vec<String>> {
    let section = |name: &str, bindings: String, flag: &str| {
        vec![
            "define-section".to_owned(),
            name.to_owned(),
            bindings,
            flag.to_owned(),
        ]
    };
    let skip = format!("script-binding {client}/skip");
    vec![
        section(KEY_SECTION, format!("ENTER {skip}\n"), "default"),
        // Ignored here, or a double click would also reach mpv's fullscreen binding.
        section(
            CLICK_SECTION,
            format!("MBTN_LEFT {skip}\nMBTN_LEFT_DBL ignore\n"),
            "force",
        ),
    ]
}

/// Command enabling or disabling an input section.
#[must_use]
pub fn set_section_command(section: &str, enabled: bool) -> Vec<String> {
    let name = if enabled {
        "enable-section"
    } else {
        "disable-section"
    };
    let mut command = vec![name.to_owned(), section.to_owned()];
    // Its mouse area is the whole window; without these flags the cursor could
    // not hide and the window could not be dragged during a segment.
    if enabled && section == KEY_SECTION {
        command.push("allow-hide-cursor+allow-vo-dragging".to_owned());
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn button_rect_scales_with_the_osd() {
        let rect = |x, y, w, h| Rect { x, y, w, h };
        assert_eq!(button_rect(1280, 720), rect(1065, 577, 172, 43));
        assert_eq!(button_rect(1920, 1080), rect(1600, 865, 256, 64));
        assert_eq!(button_rect(640, 360), rect(460, 274, 144, 36));
    }

    #[test]
    fn rect_contains_is_half_open() {
        let r = Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        };
        assert!(r.contains(10, 20), "top-left corner");
        assert!(r.contains(39, 59), "last pixel inside");
        assert!(!r.contains(40, 20), "right edge");
        assert!(!r.contains(10, 60), "bottom edge");
        assert!(!r.contains(9, 20), "left of it");
        assert!(!r.contains(10, 19), "above it");
    }

    #[test]
    fn labels_match_the_spec() {
        assert_eq!(label(SegmentKind::Intro), "Skip Intro");
        assert_eq!(label(SegmentKind::Recap), "Skip Recap");
        assert_eq!(label(SegmentKind::Outro), "Skip Credits");
        assert_eq!(label(SegmentKind::Preview), "Skip Preview");
    }

    #[test]
    fn draw_command_is_positional_and_names_the_label() {
        let command = draw_command(SegmentKind::Recap, 1280, 720, false);
        assert_eq!(command.len(), 9);
        assert_eq!(command[..3], cmd(&["osd-overlay", "1", "ass-events"]));
        assert_eq!(command[4..], cmd(&["1280", "720", "0", "no", "no"]));
        let lines: Vec<&str> = command[3].split('\n').collect();
        assert_eq!(lines.len(), 2, "one drawing line and one label line");
        assert!(lines[0].contains(r"\p1"), "the fill is a drawing");
        assert!(
            lines[1].contains(r"\b1\fs19"),
            "bold, a bit under half the height"
        );
        let (text, icon) = lines[1]
            .split_once(r"{\p1")
            .expect("an icon follows the label");
        assert!(
            text.ends_with("Skip Recap "),
            "label before the icon: {text}"
        );
        assert_eq!(icon.matches('m').count(), 2, "a triangle and a bar: {icon}");
    }

    #[test]
    fn hover_changes_the_drawn_fill() {
        let idle = draw_command(SegmentKind::Intro, 1280, 720, false);
        let hovered = draw_command(SegmentKind::Intro, 1280, 720, true);
        assert_ne!(idle[3], hovered[3]);
        assert_eq!(
            idle[3].split('\n').nth(1),
            hovered[3].split('\n').nth(1),
            "only the fill changes"
        );
    }

    #[test]
    fn clear_command_removes_the_overlay() {
        assert_eq!(
            clear_command(),
            cmd(&["osd-overlay", "1", "none", "", "0", "720", "0", "no", "no"])
        );
    }

    #[test]
    fn sections_use_default_for_keys_and_force_for_clicks() {
        assert_eq!(
            define_section_commands("mpvfin"),
            vec![
                cmd(&[
                    "define-section",
                    "mpvfin-skip-key",
                    "ENTER script-binding mpvfin/skip\n",
                    "default"
                ]),
                cmd(&[
                    "define-section",
                    "mpvfin-skip-click",
                    "MBTN_LEFT script-binding mpvfin/skip\nMBTN_LEFT_DBL ignore\n",
                    "force"
                ]),
            ]
        );
        assert_eq!(
            set_section_command(KEY_SECTION, true),
            cmd(&[
                "enable-section",
                "mpvfin-skip-key",
                "allow-hide-cursor+allow-vo-dragging"
            ])
        );
        assert_eq!(
            set_section_command(CLICK_SECTION, true),
            cmd(&["enable-section", "mpvfin-skip-click"])
        );
        assert_eq!(
            set_section_command(KEY_SECTION, false),
            cmd(&["disable-section", "mpvfin-skip-key"])
        );
        assert_eq!(
            set_section_command(CLICK_SECTION, false),
            cmd(&["disable-section", "mpvfin-skip-click"])
        );
    }
}
