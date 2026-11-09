//! Recently played: Spotify's Recents, the albums, playlists, artists and tracks played
//! lately, as cards.

use eframe::egui::{RichText, Ui};
use engine_ipc::{Page, PageReply, PageRequest};

use super::{PageCx, cards, other_page, scrolled, title};
use crate::{nav::page_name, theme};

/// What Recently played says with nothing in it.
pub const NOTHING_PLAYED: &str = "Nothing played lately.";

pub fn show(ui: &mut Ui, reply: &PageReply, cx: &mut PageCx) {
    let Page::Recents(recents) = &reply.page else {
        return other_page(ui);
    };
    scrolled(ui, cx, |ui, cx| {
        title(ui, page_name(&PageRequest::Recents));
        ui.add_space(16.0);
        if recents.is_empty() {
            ui.label(RichText::new(NOTHING_PLAYED).color(theme::SECONDARY));
        }
        cards::grid(ui, recents, cx);
    });
}
