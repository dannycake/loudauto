//! Cards: a cover 160 points wide over a title and a dim subtitle, on the surface grey.
//! A click opens the card's page (a track's plays); the play button, a white `▶` on the
//! cover's corner shown while the card is hovered, plays it. Podcasts, episodes and
//! other kinds with no page here aren't shown.

use std::{fmt::Debug, hash::Hash, sync::Arc};

use eframe::egui::{
    CursorIcon, Galley, Painter, Pos2, Rect, RichText, Role, ScrollArea, Sense, Shape, Stroke,
    StrokeKind, TextStyle, TextWrapMode, Ui, Vec2, WidgetInfo, WidgetText, pos2, vec2,
};
use engine_ipc::{PageRequest, Request};
use spot_core::pages::{Card, CardKind};

use super::{PageCx, plain_text};
use crate::{theme, track_list::liked_context};

/// Liked Songs as Home and Recently played name it.
pub const LIKED_SONGS_URI: &str = "spotify:collection:tracks";
/// A card's cover, in points.
pub const COVER: f32 = 160.0;
/// The space around a card's cover and text, in points.
const PAD: f32 = 12.0;
/// Between the cover and the title, and the title and the subtitle, in points.
const GAP: f32 = 8.0;
const LINE_GAP: f32 = 2.0;
/// Between cards, in points.
const SPACING: f32 = 12.0;
/// The play button's inset from the cover's corner, in points.
const PLAY_INSET: f32 = 8.0;

/// The page a card opens; `None` for a track, which plays instead, and for what has no
/// page here (podcasts, episodes).
pub fn card_page(card: &Card) -> Option<PageRequest> {
    if card.uri == LIKED_SONGS_URI {
        return Some(PageRequest::Liked);
    }
    let uri = card.uri.clone();
    match card.kind {
        CardKind::Album => Some(PageRequest::Album { uri }),
        CardKind::Playlist => Some(PageRequest::Playlist { uri }),
        CardKind::Artist => Some(PageRequest::Artist { uri }),
        CardKind::Track | CardKind::Other => None,
    }
}

/// What a card's play button plays; `None` when it can play nothing (Liked Songs before
/// the account's name is known; podcasts, episodes).
pub fn card_play(card: &Card, username: Option<&str>) -> Option<Request> {
    let context = |context_uri: String| Request::PlayContext {
        context_uri,
        track_uri: None,
        track_uid: None,
        shuffle: None,
    };
    if card.uri == LIKED_SONGS_URI {
        return username.map(|u| context(liked_context(u)));
    }
    match card.kind {
        CardKind::Track => Some(Request::PlayUri {
            uri: card.uri.clone(),
        }),
        CardKind::Album | CardKind::Playlist | CardKind::Artist => Some(context(card.uri.clone())),
        CardKind::Other => None,
    }
}

/// Whether a card is shown: not those of kinds with no page here.
pub fn is_shown(card: &Card) -> bool {
    card.kind != CardKind::Other
}

/// A section's title.
pub fn heading(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .text_style(TextStyle::Heading)
            .size(16.0)
            .color(theme::TEXT),
    );
    ui.add_space(4.0);
}

/// A titled row of cards, scrolled sideways; nothing for a section left with no cards.
pub fn section(ui: &mut Ui, title: &str, cards: &[Card], salt: impl Hash + Debug, cx: &mut PageCx) {
    let cards: Vec<&Card> = cards.iter().filter(|c| is_shown(c)).collect();
    if cards.is_empty() {
        return;
    }
    heading(ui, title);
    ScrollArea::horizontal()
        .id_salt(("cards", salt))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = SPACING;
                for c in cards {
                    card(ui, c, cx);
                }
            });
        });
    ui.add_space(24.0);
}

/// Cards wrapped onto as many lines as the width needs.
pub fn grid(ui: &mut Ui, cards: &[Card], cx: &mut PageCx) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(SPACING);
        for c in cards.iter().filter(|c| is_shown(c)) {
            card(ui, c, cx);
        }
    });
}

/// `text` on one line at most `width` wide, cut short with "…".
fn line(ui: &Ui, text: &str, width: f32, style: TextStyle) -> Arc<Galley> {
    WidgetText::from(text).into_galley(ui, Some(TextWrapMode::Truncate), width, style)
}

/// One card: opens its page when clicked (a track's plays); hovered, it shows its play
/// button and marks its page for the prefetch.
fn card(ui: &mut Ui, card: &Card, cx: &mut PageCx) {
    let title_height = ui.text_style_height(&TextStyle::Body);
    let subtitle_height = ui.text_style_height(&TextStyle::Small);
    let size = vec2(
        COVER + 2.0 * PAD,
        PAD + COVER + GAP + title_height + LINE_GAP + subtitle_height + PAD,
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let cover = Rect::from_min_size(rect.min + Vec2::splat(PAD), Vec2::splat(COVER));
    let enabled = ui.is_enabled();
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, &card.title));
    let play_request = card_play(card, cx.username);
    let play_centre = cover.right_bottom() - Vec2::splat(PLAY_INSET + theme::PLAY_BUTTON / 2.0);
    let play = play_request.as_ref().map(|_| {
        let play_rect = Rect::from_center_size(play_centre, Vec2::splat(theme::PLAY_BUTTON));
        let play = ui.interact(play_rect, response.id.with("play"), Sense::click());
        play.widget_info(|| {
            WidgetInfo::labeled(Role::Button, enabled, format!("Play {}", card.title))
        });
        play
    });
    let hovered = response.hovered() || play.as_ref().is_some_and(|p| p.hovered());
    let page = card_page(card);
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        if let Some(page) = &page {
            cx.hovered = Some(page.clone());
        }
    }
    if play.as_ref().is_some_and(|p| p.clicked()) {
        if let Some(req) = play_request.clone() {
            cx.send("play", req);
        }
    } else if response.clicked() {
        match page {
            Some(page) => cx.nav.go(page),
            // A track: it plays.
            None => {
                if let Some(req) = play_request.clone() {
                    cx.send("play", req);
                }
            }
        }
    }

    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter().with_clip_rect(ui.clip_rect().intersect(rect));
    let fill = if hovered {
        theme::HOVER
    } else {
        theme::SURFACE
    };
    painter.rect_filled(rect, theme::CORNER, fill);
    if response.has_focus() {
        painter.rect_stroke(rect, theme::CORNER, theme::FOCUS, StrokeKind::Inside);
    }
    // Only a cover in view is asked for.
    if ui.is_rect_visible(cover) {
        match cx.art.get(&card.images, COVER, ui.ctx()) {
            Some(texture) => {
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                painter.image(texture.id(), cover, uv, theme::UNTINTED);
            }
            None => {
                painter.rect_filled(cover, 0.0, theme::LINES);
            }
        }
    }
    let text_width = COVER;
    let title_top = cover.bottom() + GAP;
    let title = line(ui, &card.title, text_width, TextStyle::Body);
    painter.galley(pos2(cover.left(), title_top), title, theme::TEXT);
    let subtitle_text = plain_text(&card.subtitle);
    if !subtitle_text.is_empty() {
        let subtitle = line(ui, &subtitle_text, text_width, TextStyle::Small);
        let top = title_top + title_height + LINE_GAP;
        painter.galley(pos2(cover.left(), top), subtitle, theme::SECONDARY);
    }
    if hovered && play.is_some() {
        play_button(&painter, play_centre);
    }
}

/// The play button: Spotify's, the accent's disc with a black triangle on it.
fn play_button(painter: &Painter, centre: Pos2) {
    let radius = theme::PLAY_BUTTON / 2.0;
    painter.circle_filled(centre, radius, theme::ACCENT);
    let r = radius * 0.45;
    // Nudged right, so the triangle looks centred.
    let c = centre + vec2(r * 0.15, 0.0);
    let points = vec![
        c + vec2(-r * 0.6, -r),
        c + vec2(r, 0.0),
        c + vec2(-r * 0.6, r),
    ];
    painter.add(Shape::convex_polygon(
        points,
        theme::ON_ACCENT,
        Stroke::NONE,
    ));
}
