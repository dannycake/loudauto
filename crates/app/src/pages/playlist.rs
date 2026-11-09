//! A playlist: its cover, name, description and count, then its tracks with their
//! covers, which play in the playlist's context.

use eframe::egui::{RichText, Ui};
use engine_ipc::{Page, PageReply};

use super::{List, PageCx, header, other_page, plain_text, songs, title, track_list};
use crate::theme;

/// The widest a description runs before it wraps, in points.
const DESCRIPTION_WIDTH: f32 = 640.0;

pub fn show(ui: &mut Ui, reply: &PageReply, cx: &mut PageCx) {
    let Page::Playlist(list) = &reply.page else {
        return other_page(ui);
    };
    header(ui, &list.images, cx, |ui, _| {
        ui.label(RichText::new("Playlist").color(theme::SECONDARY));
        title(ui, &list.name);
        let description = plain_text(&list.description);
        if !description.is_empty() {
            ui.set_max_width(DESCRIPTION_WIDTH);
            ui.label(RichText::new(description).color(theme::SECONDARY));
        }
        ui.label(RichText::new(songs(list.total)).color(theme::SECONDARY));
    });
    ui.add_space(16.0);
    let rows = List {
        rows: &list.tracks,
        context: Some(&list.uri),
        thumbs: true,
    };
    track_list(ui, reply, rows, false, cx);
}
