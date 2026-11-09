//! An artist: the header image (dimmed) with the name over it, a Start radio button,
//! the popular tracks (playing in the artist's context), then albums, singles and the
//! artists fans also like, as cards.

use eframe::egui::{
    Rect, Role, Sense, TextStyle, TextWrapMode, Ui, Vec2, WidgetInfo, WidgetText, pos2, vec2,
};
use engine_ipc::{Page, PageReply, Request};
use spot_core::pages::ArtistPage;

use super::{List, PageCx, cards, cover, other_page, scrolled, title, track_list};
use crate::{theme, track_list::START_RADIO};

/// The avatar's side when there is no header image, in points.
const AVATAR: f32 = 160.0;

pub fn show(ui: &mut Ui, reply: &PageReply, cx: &mut PageCx) {
    let Page::Artist(artist) = &reply.page else {
        return other_page(ui);
    };
    scrolled(ui, cx, |ui, cx| {
        banner(ui, artist, cx);
        ui.add_space(12.0);
        if theme::main_button(ui, START_RADIO).clicked() {
            let seed_uri = artist.uri.clone();
            cx.send("start the radio", Request::StartRadio { seed_uri });
        }
        ui.add_space(24.0);
        if !artist.top_tracks.is_empty() {
            cards::heading(ui, "Popular");
            let list = List {
                rows: &artist.top_tracks,
                context: Some(&artist.uri),
                thumbs: true,
            };
            track_list(ui, reply, list, true, cx);
            ui.add_space(24.0);
        }
        cards::section(ui, "Albums", &artist.albums, "albums", cx);
        cards::section(ui, "Singles", &artist.singles, "singles", cx);
        cards::section(ui, "Fans also like", &artist.related, "related", cx);
    });
}

/// The header image across the page, dimmed, with the name over its lower left; with
/// none, the avatar beside the name.
fn banner(ui: &mut Ui, artist: &ArtistPage, cx: &mut PageCx) {
    if artist.header.is_empty() {
        ui.horizontal(|ui| {
            cover(ui, &artist.avatar, AVATAR, cx);
            ui.add_space(24.0);
            title(ui, &artist.name);
        });
        return;
    }
    let width = ui.available_width();
    let height = (width * 0.3).clamp(160.0, 280.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, ui.is_enabled(), &artist.name));
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter().with_clip_rect(ui.clip_rect().intersect(rect));
    match cx.art.get(&artist.header, width, ui.ctx()) {
        Some(texture) => {
            let uv = cover_uv(texture.size_vec2(), rect.size());
            painter.image(texture.id(), rect, uv, theme::UNTINTED);
            // Darkened by a layer over it, not tinted: the image keeps its colours.
            painter.rect_filled(rect, 0.0, theme::SCRIM);
        }
        None => {
            painter.rect_filled(rect, 0.0, theme::SURFACE);
        }
    }
    let name = WidgetText::from(artist.name.as_str()).into_galley(
        ui,
        Some(TextWrapMode::Truncate),
        width - 48.0,
        TextStyle::Heading,
    );
    let pos = pos2(rect.left() + 24.0, rect.bottom() - 24.0 - name.size().y);
    painter.galley(pos, name, theme::TEXT);
}

/// The part of an image `texture` big to show in a rect `rect` big, filling it without
/// stretching: the middle, cut to the rect's shape.
pub fn cover_uv(texture: Vec2, rect: Vec2) -> Rect {
    let full = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    if texture.x <= 0.0 || texture.y <= 0.0 || rect.x <= 0.0 || rect.y <= 0.0 {
        return full;
    }
    let (source, target) = (texture.x / texture.y, rect.x / rect.y);
    if source > target {
        let w = target / source;
        Rect::from_min_max(pos2((1.0 - w) / 2.0, 0.0), pos2((1.0 + w) / 2.0, 1.0))
    } else {
        let h = source / target;
        Rect::from_min_max(pos2(0.0, (1.0 - h) / 2.0), pos2(1.0, (1.0 + h) / 2.0))
    }
}
