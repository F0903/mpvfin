//! The skip button and the Next Up card on screen: drawing, hover and input.

use std::time::{Duration, Instant};

use jellyfin::SegmentKind;

use super::playlist::entry_from_uri;
use super::{NextUp, Runtime, SkipButton};
use crate::next_up;
use crate::next_up::thumbnail::Thumbnail;
use crate::runtime::values::osd_size;
use crate::server::session::Job;
use crate::skip_button::{self, CLICK_SECTION, KEY_SECTION, Rect};

/// How long the skip input stays ours after a skip press, so the second press
/// of a double press can't reach mpv's own bindings.
pub(super) const SKIP_LINGER: Duration = Duration::from_millis(400);

/// uosc's open-menu marker. It is absent before the first menu, and reads as
/// the string `null` once a menu has closed.
pub(super) const UOSC_MENU: &str = "user-data/uosc/menu/type";

impl NextUp {
    pub(super) fn thumbnail_size(&self) -> Option<(u32, u32)> {
        self.thumbnail.as_ref().map(|t| (t.width, t.height))
    }
}

impl Runtime {
    pub(super) fn expire_skip_sections(&mut self) {
        if self
            .skip_sections_off_at
            .is_some_and(|at| Instant::now() >= at)
        {
            self.skip_sections_off_at = None;
            self.run(&skip_button::set_section_command(KEY_SECTION, false));
            self.run(&skip_button::set_section_command(CLICK_SECTION, false));
        }
    }

    pub(super) fn expire_next_up_sections(&mut self) {
        if self
            .next_up_sections_off_at
            .is_some_and(|at| Instant::now() >= at)
        {
            self.next_up_sections_off_at = None;
            self.run(&skip_button::set_section_command(
                next_up::KEY_SECTION,
                false,
            ));
            self.run(&skip_button::set_section_command(
                next_up::CLICK_SECTION,
                false,
            ));
        }
    }

    /// Defines the button's input sections; they stay inert until enabled.
    pub(super) fn define_skip_sections(&self) {
        let Some(client) = self.mpv.client_name() else {
            self.mpv.log("mpv reported no client name: no skip input");
            return;
        };
        for command in skip_button::define_section_commands(&client)
            .into_iter()
            .chain(next_up::define_section_commands(&client))
        {
            self.run(&command);
        }
    }

    pub(super) fn show_skip_button(&mut self, kind: SegmentKind) {
        // A pending disable ends here; the click section must follow the pointer again.
        if self.skip_sections_off_at.take().is_some() {
            self.run(&skip_button::set_section_command(CLICK_SECTION, false));
        }
        self.skip_button = Some(SkipButton {
            kind,
            hovered: false,
        });
        self.run(&skip_button::set_section_command(KEY_SECTION, true));
        self.draw_skip_button();
        // The pointer may already be there, and a still pointer sends no change.
        self.update_skip_hover();
    }

    pub(super) fn hide_skip_button(&mut self) {
        self.skip_button = None;
        self.run(&skip_button::clear_command());
        // At once, unless the press that hid the button pushes this back.
        self.skip_sections_off_at = Some(Instant::now());
    }

    pub(super) fn draw_skip_button(&self) {
        if let Some(button) = self.skip_button {
            let (w, h) = osd_size(&self.observed);
            self.run(&skip_button::draw_command(
                button.kind,
                w,
                h,
                button.hovered,
            ));
        }
    }

    pub(super) fn overlays_resized(&mut self) {
        self.draw_skip_button();
        self.draw_next_up();
        self.place_thumbnail();
        self.update_hover();
    }

    pub(super) fn update_hover(&mut self) {
        self.update_skip_hover();
        self.update_next_up_hover();
    }

    /// Follows mpv's playlist, whoever changes it, while the card is due.
    pub(super) fn refresh_next_up(&mut self) {
        let Some(card) = &self.next_up else {
            return;
        };
        let on_screen = card.label.is_some();
        let next = self.next_playlist_item();
        if next == card.item_id {
            return;
        }
        if on_screen {
            self.remove_next_up_card();
        }
        if let Some(item_id) = next.clone() {
            self.session.submit(Job::NextUpCard { item_id });
        }
        self.next_up = Some(NextUp {
            item_id: next,
            ..NextUp::default()
        });
    }

    pub(super) fn next_playlist_item(&self) -> Option<String> {
        let playing = self.mpv.get_property("playlist-playing-pos")?;
        let next = playing.parse::<i64>().ok()? + 1;
        let uri = self
            .mpv
            .get_property(&format!("playlist/{next}/filename"))?;
        entry_from_uri(&uri).map(|(item_id, _)| item_id.to_owned())
    }

    pub(super) fn next_up_on_screen(&self) -> bool {
        self.next_up.as_ref().is_some_and(|c| c.label.is_some())
    }

    pub(super) fn next_up_ready(
        &mut self,
        item_id: &str,
        label: String,
        thumbnail: Option<Thumbnail>,
    ) {
        let Some(card) = self
            .next_up
            .as_mut()
            .filter(|card| card.item_id.as_deref() == Some(item_id))
        else {
            return;
        };
        card.label = Some(label);
        card.thumbnail = thumbnail;
        // A pending disable ends here; the click section must follow the pointer again.
        if self.next_up_sections_off_at.take().is_some() {
            self.run(&skip_button::set_section_command(
                next_up::CLICK_SECTION,
                false,
            ));
        }
        self.run(&skip_button::set_section_command(
            next_up::KEY_SECTION,
            true,
        ));
        self.draw_next_up();
        self.place_thumbnail();
        // The pointer may already be there, and a still pointer sends no change.
        self.update_next_up_hover();
    }

    /// The input goes at once, unless the press that hid the card pushes this back.
    pub(super) fn remove_next_up_card(&mut self) {
        self.run(&next_up::clear_command());
        if self
            .next_up
            .as_ref()
            .is_some_and(|card| card.thumbnail.is_some())
        {
            self.run(&next_up::remove_thumbnail_command());
        }
        self.next_up_sections_off_at = Some(Instant::now());
    }

    pub(super) fn next_up_layout(&self) -> Option<next_up::Layout> {
        let card = self.next_up.as_ref()?;
        let (w, h) = osd_size(&self.observed);
        Some(next_up::layout(
            card.label.as_ref()?,
            card.thumbnail_size(),
            w,
            h,
        ))
    }

    pub(super) fn draw_next_up(&self) {
        if let Some(card) = &self.next_up
            && let Some(label) = &card.label
        {
            let (w, h) = osd_size(&self.observed);
            self.run(&next_up::draw_command(
                label,
                card.thumbnail_size(),
                w,
                h,
                card.hovered,
            ));
        }
    }

    pub(super) fn place_thumbnail(&self) {
        if let Some(card) = &self.next_up
            && let Some(picture) = &card.thumbnail
            && let Some(at) = self.next_up_layout().and_then(|layout| layout.thumbnail)
        {
            let size = (picture.width, picture.height);
            self.run(&next_up::thumbnail_command(&picture.bgra, size, at));
        }
    }

    /// Rereads the pointer while the card is up; when it crosses Play Now's
    /// edge, moves the click section and redraws.
    pub(super) fn update_next_up_hover(&mut self) {
        let (Some(layout), Some(was)) = (
            self.next_up_layout(),
            self.next_up.as_ref().map(|card| card.hovered),
        ) else {
            return;
        };
        let hovered = !self.uosc_menu_open() && self.pointer_over(layout.play);
        if hovered != was {
            if let Some(card) = &mut self.next_up {
                card.hovered = hovered;
            }
            self.run(&skip_button::set_section_command(
                next_up::CLICK_SECTION,
                hovered,
            ));
            self.draw_next_up();
        }
    }

    /// Rereads the pointer while a button is up; when it crosses the button's
    /// edge, moves the click section and redraws.
    pub(super) fn update_skip_hover(&mut self) {
        let Some(button) = self.skip_button else {
            return;
        };
        let (w, h) = osd_size(&self.observed);
        let hovered = !self.uosc_menu_open() && self.pointer_over(skip_button::button_rect(w, h));
        if hovered != button.hovered {
            self.skip_button = Some(SkipButton { hovered, ..button });
            self.run(&skip_button::set_section_command(CLICK_SECTION, hovered));
            self.draw_skip_button();
        }
    }

    /// uosc's menu draws over the buttons and owns the click there.
    pub(super) fn uosc_menu_open(&self) -> bool {
        self.mpv
            .get_property(UOSC_MENU)
            .is_some_and(|menu| menu != "null")
    }

    pub(super) fn pointer_over(&self, rect: Rect) -> bool {
        let coordinate = |name| self.mpv.get_property(name)?.parse::<i64>().ok();
        self.mpv.get_property("mouse-pos/hover").as_deref() == Some("yes")
            && coordinate("mouse-pos/x")
                .zip(coordinate("mouse-pos/y"))
                .is_some_and(|(x, y)| rect.contains(x, y))
    }
}
