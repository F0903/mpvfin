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
        SegmentKind::Intro => "Skip intro",
        SegmentKind::Recap => "Skip recap",
        SegmentKind::Outro => "Skip credits",
        SegmentKind::Preview => "Skip preview",
    }
}

/// `osd-overlay` command drawing the button.
#[must_use]
pub fn draw_command(kind: SegmentKind, osd_w: i64, osd_h: i64, hovered: bool) -> Vec<String> {
    let Rect { x, y, w, h } = button_rect(osd_w, osd_h);
    let fill = if hovered { "606060" } else { "202020" };
    let r = h / 4;
    let (right, bottom) = (w - r, h - r);
    let shape = format!(
        "m {r} 0 l {right} 0 b {w} 0 {w} 0 {w} {r} l {w} {bottom} b {w} {h} {w} {h} {right} {h} \
         l {r} {h} b 0 {h} 0 {h} 0 {bottom} l 0 {r} b 0 0 0 0 {r} 0"
    );
    let (center_x, center_y, font_size, text) = (x + w / 2, y + h / 2, h / 2, label(kind));
    let data = format!(
        "{{\\an7\\pos({x},{y})\\bord0\\shad0\\1c&H{fill}&\\1a&H40&\\p1}}{shape}{{\\p0}}\n\
         {{\\an5\\pos({center_x},{center_y})\\bord0\\shad0\\1c&HFFFFFF&\\1a&H00&\\fs{font_size}}}{text}"
    );
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
    let section = |name: &str, key: &str, flag: &str| {
        vec![
            "define-section".to_owned(),
            name.to_owned(),
            format!("{key} script-binding {client}/skip\n"),
            flag.to_owned(),
        ]
    };
    vec![
        section(KEY_SECTION, "ENTER", "default"),
        section(CLICK_SECTION, "MBTN_LEFT", "force"),
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

/// Whether a client message is a press of the `skip` binding: state `d` is a
/// key going down, `p` a press whose up and down can't be told apart.
#[must_use]
pub fn is_skip_press(args: &[String]) -> bool {
    matches!(args, [message, name, state, ..]
        if message == "key-binding" && name == "skip" && state.starts_with(['d', 'p']))
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
        assert_eq!(label(SegmentKind::Intro), "Skip intro");
        assert_eq!(label(SegmentKind::Recap), "Skip recap");
        assert_eq!(label(SegmentKind::Outro), "Skip credits");
        assert_eq!(label(SegmentKind::Preview), "Skip preview");
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
        assert!(lines[1].contains(r"\fs21"), "font size is half the height");
        assert!(lines[1].ends_with("Skip recap"));
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
                    "MBTN_LEFT script-binding mpvfin/skip\n",
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

    #[test]
    fn only_presses_of_skip_count() {
        let cases: &[(&[&str], bool)] = &[
            (&["key-binding", "skip", "d--", "ENTER"], true),
            (&["key-binding", "skip", "p--", "ENTER"], true),
            (&["key-binding", "skip", "dm-", "MBTN_LEFT"], true),
            (&["key-binding", "skip", "r--", "ENTER"], false),
            (&["key-binding", "skip", "u--", "ENTER"], false),
            (&["key-binding", "other", "d--", "ENTER"], false),
            (&["something-else", "skip", "d--"], false),
            (&["key-binding", "skip"], false),
            (&[], false),
        ];
        for (args, expected) in cases {
            assert_eq!(is_skip_press(&cmd(args)), *expected, "{args:?}");
        }
    }
}
