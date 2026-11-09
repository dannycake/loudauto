//! Liked Songs: the count in the header, then the tracks with their covers, which play
//! in the account's collection (`spotify:user:<username>:collection`). Every row is
//! liked by definition.

use eframe::egui::{RichText, Ui};
use engine_ipc::{Page, PageReply, PageRequest};

use super::{List, PageCx, other_page, songs, title, track_list};
use crate::{nav::page_name, theme, track_list::liked_context};

pub fn show(ui: &mut Ui, reply: &PageReply, cx: &mut PageCx) {
    let Page::Liked(liked) = &reply.page else {
        return other_page(ui);
    };
    title(ui, page_name(&PageRequest::Liked));
    ui.label(RichText::new(songs(liked.total)).color(theme::SECONDARY));
    ui.add_space(16.0);
    // Until the engine says whose they are, rows play alone.
    let context = cx.username.map(liked_context);
    let list = List {
        rows: &liked.tracks,
        context: context.as_deref(),
        thumbs: true,
    };
    track_list(ui, reply, list, false, cx);
}
