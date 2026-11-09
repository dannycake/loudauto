//! The track list: a playlist's, an album's, Liked Songs', search's and an artist's rows.
//! Virtualized: only the rows in view are laid out, and only they ask for their covers.
//! The playing row has its title and a play glyph for its number in the accent, and a 3 px
//! accent stripe down its left edge; the selected row the quiet accent fill, and a 1 px
//! accent outline while the list has keyboard focus. Double-click or Enter plays the list's
//! context from that row. Artist and album names are links: white and underlined while
//! hovered.

use std::sync::Arc;

use eframe::egui::{
    Align, Align2, Button, Color32, CursorIcon, EventFilter, FontId, Galley, Id, Key, Modifiers,
    Painter, Pos2, Rect, Response, Role, ScrollArea, Sense, Stroke, StrokeKind, TextStyle,
    TextWrapMode, Ui, WidgetInfo, WidgetText, pos2, vec2,
};
use engine_ipc::{PlayerState, Request};
use spot_core::pages::TrackRow;

use crate::{
    art::Art,
    theme::{self, ROW_HEIGHT, THUMB},
};

/// The list's accessible name.
pub const LIST_LABEL: &str = "Tracks";
/// The heart's accessible name; whether it is toggled says whether the track is liked.
pub const HEART_LABEL: &str = "Like";

/// The right-click menu, in order.
pub const ADD_TO_QUEUE: &str = "Add to queue";
pub const START_RADIO: &str = "Start radio";
pub const GO_TO_ARTIST: &str = "Go to artist";
pub const GO_TO_ALBUM: &str = "Go to album";
pub const SHOW_CREDITS: &str = "Show credits";
pub const COPY_LINK: &str = "Copy Spotify link";

const PLAY_GLYPH: &str = "▶";
const LIKED_GLYPH: &str = "♥";
const NOT_LIKED_GLYPH: &str = "♡";

/// The space before the number and after the heart, in points.
const PAD: f32 = 16.0;
const END_PAD: f32 = 8.0;
/// The space between columns, in points.
const GAP: f32 = 12.0;
/// The fixed columns' widths, in points.
const NUMBER_WIDTH: f32 = 40.0;
const DURATION_WIDTH: f32 = 48.0;
const HEART_WIDTH: f32 = 40.0;
/// The room title, artists and album share, from which the album column shows, and the
/// artists column, in points: a narrow window keeps the title readable.
const ALBUM_FROM: f32 = 400.0;
const ARTISTS_FROM: f32 = 160.0;

/// A list of tracks, drawn by [`TrackList::show`].
pub struct TrackList<'a> {
    /// Tells this list's scroll position and focus apart from every other list's: the
    /// page's key, the same on every frame.
    pub id_salt: &'a str,
    pub rows: &'a [TrackRow],
    /// The context the rows play in (the playlist, the album, Liked Songs); `None` where
    /// a row plays alone (search results).
    pub context_uri: Option<&'a str>,
    /// Whether rows show their cover beside the title (not on album pages).
    pub show_thumbs: bool,
    /// Whether a track is liked; `None` while unknown, drawn as not liked.
    pub liked: &'a dyn Fn(&str) -> Option<bool>,
    /// The caller's copy of the scroll offset: shown from on the list's first frame, or
    /// whenever the caller changes it; the offset is written back on every frame.
    pub scroll: Option<&'a mut f32>,
}

/// Which row is selected, whether the list has keyboard focus, and which link the
/// pointer is over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub index: Option<usize>,
    /// Whether the list had keyboard focus at the end of its last frame: written by
    /// [`TrackList::show`].
    pub focused: bool,
    /// The artist or album name the pointer is over this frame, if any: written by
    /// [`TrackList::show`] on every frame, for the caller to prefetch that page.
    pub hovered_link: Option<LinkTarget>,
}

/// Where an artist or album name in a row links to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Artist(String),
    Album(String),
}

impl LinkTarget {
    /// What clicking the link does.
    fn action(&self) -> RowAction {
        match self {
            LinkTarget::Artist(uri) => RowAction::GoArtist(uri.clone()),
            LinkTarget::Album(uri) => RowAction::GoAlbum(uri.clone()),
        }
    }
}

/// What the owner did to a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowAction {
    /// Play the list's context from row `index` (see [`play_request`]).
    Play { index: usize },
    /// Make this track liked or not: the state its heart didn't show (an unknown heart
    /// is drawn as not liked, so it likes).
    Heart { uri: String, liked: bool },
    /// Add this track to the queue.
    AddToQueue(String),
    /// Start a radio from this track.
    StartRadio(String),
    /// Open this artist's page.
    GoArtist(String),
    /// Open this album's page.
    GoAlbum(String),
    /// Show this track's credits.
    ShowCredits(String),
    /// This open.spotify.com link was copied to the clipboard.
    CopyLink(String),
}

/// What a list remembers between frames, in egui's memory.
#[derive(Debug, Clone, Copy)]
struct Memory {
    /// The scroll offset at the end of the last frame, as written to the caller's copy.
    offset: f32,
    /// The height of the rows' viewport, last frame.
    viewport: f32,
}

/// What every row of one frame is drawn with.
struct Rows<'a> {
    list_id: Id,
    context_uri: Option<&'a str>,
    show_thumbs: bool,
    liked: &'a dyn Fn(&str) -> Option<bool>,
    player: &'a PlayerState,
    focused: bool,
}

impl TrackList<'_> {
    /// Draws the list into the rest of `ui`; returns what the owner did this frame.
    pub fn show(
        self,
        ui: &mut Ui,
        player: &PlayerState,
        sel: &mut Selection,
        art: &mut Art,
    ) -> Option<RowAction> {
        let TrackList {
            id_salt,
            rows,
            context_uri,
            show_thumbs,
            liked,
            scroll,
        } = self;
        let list_id = ui.make_persistent_id(("track-list", id_salt));
        let memory: Option<Memory> = ui.ctx().data(|d| d.get_temp(list_id));
        let mut action = None;
        sel.hovered_link = None;

        // Rows that went away (a track unliked off Liked Songs) take the selection with
        // them only as far as the last row.
        sel.index = match rows.len() {
            0 => None,
            n => sel.index.map(|i| i.min(n - 1)),
        };

        // The list is one focus target, not one per row: rows come and go as they scroll
        // by, and egui drops the focus of a widget it no longer sees.
        let bounds = ui.available_rect_before_wrap();
        let focus = ui.interact(bounds, list_id, Sense::FOCUSABLE);
        focus.widget_info(|| WidgetInfo::labeled(Role::List, ui.is_enabled(), LIST_LABEL));
        let focused = focus.has_focus();

        // Shown from the caller's offset on the first frame, or when the caller moved it.
        let mut target = match (scroll.as_deref(), memory) {
            (Some(&offset), None) => Some(offset),
            (Some(&offset), Some(m)) if offset != m.offset => Some(offset),
            _ => None,
        };

        if focused {
            // ↑ and ↓ move the selection, not egui's focus.
            let filter = EventFilter {
                vertical_arrows: true,
                ..Default::default()
            };
            ui.memory_mut(|m| m.set_focus_lock_filter(list_id, filter));
            let (down, up, enter) = ui.input_mut(|i| {
                (
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                )
            });
            if let Some(last) = rows.len().checked_sub(1) {
                if down + up > 0 {
                    let index = match sel.index {
                        Some(i) => (i + down).saturating_sub(up).min(last),
                        None => down.saturating_sub(1).min(last),
                    };
                    sel.index = Some(index);
                    if let Some(m) = memory {
                        target = Some(into_view(index, target.unwrap_or(m.offset), m.viewport));
                    }
                }
                if enter && let Some(index) = sel.index {
                    action = Some(RowAction::Play { index });
                }
            }
        }

        let draw = Rows {
            list_id,
            context_uri,
            show_thumbs,
            liked,
            player,
            focused,
        };
        let mut scroll_area = ScrollArea::vertical()
            .id_salt(("track-list", id_salt))
            .auto_shrink(false);
        if let Some(offset) = target {
            scroll_area = scroll_area.vertical_scroll_offset(offset);
        }
        let output = ui
            .scope(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                scroll_area.show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
                    for index in range {
                        if let Some(a) = draw.row(ui, index, &rows[index], sel, art) {
                            action = Some(a);
                        }
                    }
                })
            })
            .inner;

        let offset = output.state.clamped_offset().y;
        if let Some(scroll) = scroll {
            *scroll = offset;
        }
        let viewport = output.inner_rect.height();
        ui.ctx()
            .data_mut(|d| d.insert_temp(list_id, Memory { offset, viewport }));
        sel.focused = ui.memory(|m| m.has_focus(list_id));
        action
    }
}

impl Rows<'_> {
    /// Row `index`: one line, `ROW_HEIGHT` tall whatever its text.
    fn row(
        &self,
        ui: &mut Ui,
        index: usize,
        row: &TrackRow,
        sel: &mut Selection,
        art: &mut Art,
    ) -> Option<RowAction> {
        let mut action = None;
        let (_, rect) = ui.allocate_space(vec2(ui.available_width(), ROW_HEIGHT));
        let id = self.list_id.with(index);
        let enabled = ui.is_enabled();
        let label = if row.title.is_empty() {
            &row.uri
        } else {
            &row.title
        };

        let response = ui.interact(rect, id, Sense::CLICK);
        let cols = Columns::new(rect, self.show_thumbs);
        // The heart and the links go on top of the row, so a click on one is its alone.
        let heart = ui.interact(cols.heart, id.with("heart"), Sense::CLICK);
        let mut names = Vec::new();
        if let Some(cell) = cols.artists {
            let artists = row.artists.iter().map(|a| {
                let target = is_page(&a.uri, "artist").then(|| LinkTarget::Artist(a.uri.clone()));
                (a.name.as_str(), target)
            });
            names.extend(lay_out_names(ui, cell, artists));
        }
        if let (Some(cell), Some(album)) = (cols.album, &row.album) {
            let target = is_page(&album.uri, "album").then(|| LinkTarget::Album(album.uri.clone()));
            names.extend(lay_out_names(ui, cell, [(album.name.as_str(), target)]));
        }
        let links: Vec<Option<Response>> = names
            .iter()
            .enumerate()
            .map(|(k, name)| {
                name.target.as_ref()?;
                let link = ui.interact(name.rect(), id.with(("link", k)), Sense::CLICK);
                let text = name.galley.text();
                link.widget_info(|| WidgetInfo::labeled(Role::Link, enabled, text));
                Some(link)
            })
            .collect();

        let liked = (self.liked)(&row.uri) == Some(true);
        if heart.clicked() {
            action = Some(RowAction::Heart {
                uri: row.uri.clone(),
                liked: !liked,
            });
        }
        for (name, link) in names.iter().zip(&links) {
            if let (Some(target), Some(link)) = (&name.target, link) {
                if link.clicked() {
                    action = Some(target.action());
                }
                if link.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    sel.hovered_link = Some(target.clone());
                }
            }
        }
        let overlays: Vec<&Response> = std::iter::once(&heart)
            .chain(links.iter().flatten())
            .collect();
        if response.clicked()
            || response.secondary_clicked()
            || overlays.iter().any(|r| r.secondary_clicked())
        {
            sel.index = Some(index);
        }
        if response.clicked() {
            ui.memory_mut(|m| m.request_focus(self.list_id));
        }
        if response.double_clicked() {
            action = Some(RowAction::Play { index });
        }
        // A right-click on the heart or a link offers the row's menu too.
        for r in std::iter::once(&response).chain(overlays.iter().copied()) {
            r.context_menu(|ui| {
                if let Some(a) = menu(ui, row) {
                    action = Some(a);
                }
            });
        }
        let selected = sel.index == Some(index);
        response.widget_info(|| WidgetInfo::selected(Role::ListItem, enabled, selected, label));
        heart.widget_info(|| WidgetInfo::selected(Role::Button, enabled, liked, HEART_LABEL));

        if !ui.is_rect_visible(rect) {
            return action;
        }
        let painter = ui.painter().with_clip_rect(rect);
        if selected {
            painter.rect_filled(rect, 0.0, theme::SELECTED);
        } else if response.hovered() || overlays.iter().any(|r| r.hovered()) {
            painter.rect_filled(rect, 0.0, theme::HOVER);
        }
        let playing = is_playing_row(row, self.context_uri, self.player);
        if playing {
            let stripe = Rect::from_min_size(rect.min, vec2(theme::STRIPE_WIDTH, rect.height()));
            painter.rect_filled(stripe, 0.0, theme::ACCENT);
        }
        if selected && self.focused {
            painter.rect_stroke(rect, 0.0, theme::FOCUS, StrokeKind::Inside);
        }

        if playing {
            glyph_cell(&painter, PLAY_GLYPH, cols.number, Align::Max, theme::ACCENT);
        } else {
            let number = (index + 1).to_string();
            cell(
                ui,
                &painter,
                &number,
                cols.number,
                Align::Max,
                theme::SECONDARY,
            );
        }
        if let Some(thumb) = cols.thumb {
            match art.get(&row.images, THUMB, ui.ctx()) {
                Some(cover) => {
                    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                    painter.image(cover.id(), thumb, uv, theme::UNTINTED);
                }
                None => {
                    painter.rect_filled(thumb, 0.0, theme::LINES);
                }
            }
        }
        let title = if playing { theme::ACCENT } else { theme::TEXT };
        cell(ui, &painter, &row.title, cols.title, Align::Min, title);
        for (name, link) in names.into_iter().zip(&links) {
            let hovered = link.as_ref().is_some_and(Response::hovered);
            let idle = theme::SECONDARY;
            paint_link(&painter, name.cell, name.pos, name.galley, (idle, hovered));
        }
        cell(
            ui,
            &painter,
            &duration(row.duration_ms),
            cols.duration,
            Align::Max,
            theme::SECONDARY,
        );
        let (glyph, colour) = match liked {
            true => (LIKED_GLYPH, theme::ACCENT),
            false if heart.hovered() => (NOT_LIKED_GLYPH, theme::TEXT),
            false => (NOT_LIKED_GLYPH, theme::LINES),
        };
        glyph_cell(&painter, glyph, cols.heart, Align::Center, colour);
        action
    }
}

/// The right-click menu's entries; an entry the row has nothing for is disabled.
fn menu(ui: &mut Ui, row: &TrackRow) -> Option<RowAction> {
    let mut action = None;
    let uri = || row.uri.clone();
    if ui.button(ADD_TO_QUEUE).clicked() {
        action = Some(RowAction::AddToQueue(uri()));
    }
    if ui.button(START_RADIO).clicked() {
        action = Some(RowAction::StartRadio(uri()));
    }
    let artist = row.artists.iter().find(|a| is_page(&a.uri, "artist"));
    if ui
        .add_enabled(artist.is_some(), Button::new(GO_TO_ARTIST))
        .clicked()
        && let Some(artist) = artist
    {
        action = Some(RowAction::GoArtist(artist.uri.clone()));
    }
    let album = row.album.as_ref().filter(|a| is_page(&a.uri, "album"));
    if ui
        .add_enabled(album.is_some(), Button::new(GO_TO_ALBUM))
        .clicked()
        && let Some(album) = album
    {
        action = Some(RowAction::GoAlbum(album.uri.clone()));
    }
    if ui.button(SHOW_CREDITS).clicked() {
        action = Some(RowAction::ShowCredits(uri()));
    }
    let link = web_link(&row.uri);
    if ui
        .add_enabled(link.is_some(), Button::new(COPY_LINK))
        .clicked()
        && let Some(link) = link
    {
        ui.ctx().copy_text(link.clone());
        action = Some(RowAction::CopyLink(link));
    }
    action
}

/// Where a row's cells sit. The album column, then the artists', give way first when
/// the list is narrow.
struct Columns {
    number: Rect,
    thumb: Option<Rect>,
    title: Rect,
    artists: Option<Rect>,
    album: Option<Rect>,
    duration: Rect,
    heart: Rect,
}

impl Columns {
    fn new(row: Rect, thumbs: bool) -> Columns {
        let column = |left: f32, width: f32| {
            Rect::from_x_y_ranges(left..=left + width.max(0.0), row.y_range())
        };
        let mut left = row.left() + PAD;
        let number = column(left, NUMBER_WIDTH);
        left += NUMBER_WIDTH + GAP;
        let thumb = thumbs.then(|| {
            let thumb = Rect::from_center_size(
                pos2(left + THUMB / 2.0, row.center().y),
                vec2(THUMB, THUMB),
            );
            left += THUMB + GAP;
            thumb
        });
        let mut right = row.right() - END_PAD - HEART_WIDTH;
        let heart = column(right, HEART_WIDTH);
        right -= DURATION_WIDTH;
        let duration = column(right, DURATION_WIDTH);
        right -= GAP;

        let room = (right - left).max(0.0);
        let (title, artists, album) = if room >= ALBUM_FROM {
            let share = room - 2.0 * GAP;
            let (t, a) = (share * 0.4, share * 0.3);
            (
                column(left, t),
                Some(column(left + t + GAP, a)),
                Some(column(left + t + a + 2.0 * GAP, share - t - a)),
            )
        } else if room >= ARTISTS_FROM {
            let share = room - GAP;
            let t = share * 0.55;
            (
                column(left, t),
                Some(column(left + t + GAP, share - t)),
                None,
            )
        } else {
            (column(left, room), None, None)
        };
        Columns {
            number,
            thumb,
            title,
            artists,
            album,
            duration,
            heart,
        }
    }
}

/// One laid-out piece of the artists' or the album's cell: a name, or the ", " between
/// two artists.
struct Name {
    pos: Pos2,
    galley: Arc<Galley>,
    /// The cell it is clipped to.
    cell: Rect,
    /// Where it links to; `None` for a separator, or a name with no page (a local file's).
    target: Option<LinkTarget>,
}

impl Name {
    fn rect(&self) -> Rect {
        Rect::from_min_size(self.pos, self.galley.size())
    }
}

/// `names` in a row along `cell`, ", " between them, centred down; the one that doesn't
/// fit is cut short with "…" and the rest left out. A name that is empty is skipped.
fn lay_out_names<'a>(
    ui: &Ui,
    cell: Rect,
    names: impl IntoIterator<Item = (&'a str, Option<LinkTarget>)>,
) -> Vec<Name> {
    let mut out = Vec::new();
    let mut x = cell.left();
    let mut place = |text: &str, target: Option<LinkTarget>, x: &mut f32| -> bool {
        let room = cell.right() - *x;
        if room <= 0.0 {
            return false;
        }
        let galley = galley(ui, text, room);
        let size = galley.size();
        let elided = galley.elided;
        out.push(Name {
            pos: pos2(*x, cell.center().y - size.y / 2.0),
            galley,
            cell,
            target,
        });
        *x += size.x;
        !elided
    };
    let names = names.into_iter().filter(|(name, _)| !name.is_empty());
    for (i, (name, target)) in names.enumerate() {
        if i > 0 && !place(", ", None, &mut x) {
            break;
        }
        if !place(name, target, &mut x) {
            break;
        }
    }
    out
}

/// Whether `uri` is a `spotify:<kind>:<id>` page.
pub(crate) fn is_page(uri: &str, kind: &str) -> bool {
    uri.strip_prefix("spotify:")
        .and_then(|rest| rest.strip_prefix(kind))
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(|id| !id.is_empty() && !id.contains(':'))
}

/// `text` as one line at most `width` wide, cut short with "…".
pub(crate) fn galley(ui: &Ui, text: &str, width: f32) -> Arc<Galley> {
    WidgetText::from(text).into_galley(ui, Some(TextWrapMode::Truncate), width, TextStyle::Body)
}

/// The look of a link (spec "Links"), shared by track rows, the pages' links and the
/// bottom bar: `galley` at `pos`, clipped to its `cell`, in `idle`; white, with a 1 px
/// white underline, while `hovered` (Spotify's own way).
pub(crate) fn paint_link(
    painter: &Painter,
    cell: Rect,
    pos: Pos2,
    galley: Arc<Galley>,
    (idle, hovered): (Color32, bool),
) {
    let painter = painter.with_clip_rect(cell);
    let rect = Rect::from_min_size(pos, galley.size());
    if hovered {
        painter.galley(pos, galley, theme::TEXT);
        let underline = Stroke::new(1.0, theme::TEXT);
        painter.hline(rect.x_range(), rect.bottom() - 0.5, underline);
    } else {
        painter.galley(pos, galley, idle);
    }
}

/// `text` on one line in `cell`, cut short with "…" where it doesn't fit; centred down.
fn cell(ui: &Ui, painter: &Painter, text: &str, cell: Rect, align: Align, colour: Color32) {
    if text.is_empty() || cell.width() <= 0.0 {
        return;
    }
    let galley = galley(ui, text, cell.width());
    let size = galley.size();
    let x = match align {
        Align::Min => cell.left(),
        Align::Center => cell.center().x - size.x / 2.0,
        Align::Max => cell.right() - size.x,
    };
    painter.galley(pos2(x, cell.center().y - size.y / 2.0), galley, colour);
}

/// A glyph (the heart, the playing row's ▶) in `cell`, at the icons' size for a row, a
/// third bigger than the text; centred down.
fn glyph_cell(painter: &Painter, glyph: &str, cell: Rect, align: Align, colour: Color32) {
    let font = FontId::proportional(theme::ICON_SMALL);
    let y = cell.center().y;
    let (anchor, x) = match align {
        Align::Min => (Align2::LEFT_CENTER, cell.left()),
        Align::Center => (Align2::CENTER_CENTER, cell.center().x),
        Align::Max => (Align2::RIGHT_CENTER, cell.right()),
    };
    painter.text(pos2(x, y), anchor, glyph, font, colour);
}

/// The offset that shows row `index` whole, moving as little as it can from `offset`.
fn into_view(index: usize, offset: f32, viewport: f32) -> f32 {
    let top = index as f32 * ROW_HEIGHT;
    let bottom = top + ROW_HEIGHT;
    if top < offset {
        top
    } else if bottom > offset + viewport {
        (bottom - viewport).max(0.0)
    } else {
        offset
    }
}

/// Whether `row` is what's playing: the same track, and in a list with a context, the
/// same context.
pub fn is_playing_row(row: &TrackRow, ctx: Option<&str>, p: &PlayerState) -> bool {
    p.track.as_ref().is_some_and(|t| t.uri == row.uri)
        && ctx.is_none_or(|ctx| p.context_uri.as_deref() == Some(ctx))
}

/// A duration as `m:ss`, the seconds rounded down.
pub fn duration(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// What plays row `index` of `rows`: its list's context from that row, told apart from
/// another copy of the track by its uid; with no context (search results), the track
/// alone. `None` past the end.
pub fn play_request(rows: &[TrackRow], context_uri: Option<&str>, index: usize) -> Option<Request> {
    let row = rows.get(index)?;
    Some(match context_uri {
        Some(context_uri) => Request::PlayContext {
            context_uri: context_uri.to_owned(),
            track_uri: Some(row.uri.clone()),
            track_uid: row.uid.clone(),
            shuffle: None,
        },
        None => Request::PlayUri {
            uri: row.uri.clone(),
        },
    })
}

/// The context Liked Songs play in.
pub fn liked_context(username: &str) -> String {
    format!("spotify:user:{username}:collection")
}

/// A track's (or an episode's) open.spotify.com link; `None` for what has no page there,
/// such as a local file.
pub fn web_link(uri: &str) -> Option<String> {
    let mut parts = uri.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("spotify"), Some(kind @ ("track" | "episode")), Some(id), None) if !id.is_empty() => {
            Some(format!("https://open.spotify.com/{kind}/{id}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use eframe::egui::{
        self, Color32, CursorIcon, Event, Modifiers, MouseWheelUnit, OutputCommand, PointerButton,
        Pos2, Rect, Shape, Stroke, TextStyle,
        accesskit::{Role, Toggled},
        pos2, vec2,
    };
    use egui_kittest::{
        Harness, HarnessBuilder,
        kittest::{NodeT, Queryable},
    };
    use engine_ipc::Request;
    use playback::{
        command::Repeat,
        event::{Actions, NowPlaying, PlayStatus},
    };
    use spot_core::pages::Named;

    use super::*;
    use crate::{
        test_support::{self, GLYPHS, fills, size_of, texts},
        theme,
    };

    const PLAYLIST: &str = "spotify:playlist:p";

    fn row(uri: &str, uid: Option<&str>, title: &str) -> TrackRow {
        TrackRow {
            uri: uri.into(),
            uid: uid.map(Into::into),
            title: title.into(),
            artists: vec![Named {
                uri: "spotify:artist:jr".into(),
                name: "Jane Remover".into(),
            }],
            album: Some(Named {
                uri: "spotify:album:cd".into(),
                name: "Census Designated".into(),
            }),
            duration_ms: 230_526,
            images: vec![],
            added_at: None,
        }
    }

    /// Rows titled `Song 0`, `Song 1`, …
    fn songs(n: usize) -> Vec<TrackRow> {
        (0..n)
            .map(|i| row(&format!("spotify:track:s{i}"), None, &format!("Song {i}")))
            .collect()
    }

    /// Alpha, Beta, Gamma: tracks `a`, `b`, `c`.
    fn three() -> Vec<TrackRow> {
        vec![
            row("spotify:track:a", Some("ua"), "Alpha"),
            row("spotify:track:b", Some("ub"), "Beta"),
            row("spotify:track:c", Some("uc"), "Gamma"),
        ]
    }

    fn player(track: Option<&str>, context: Option<&str>) -> PlayerState {
        PlayerState {
            track: track.map(|uri| NowPlaying {
                uri: uri.into(),
                title: "t".into(),
                artists: vec![],
                artist_uris: vec![],
                album: None,
                album_uri: None,
                duration_ms: None,
                cover_url: None,
            }),
            status: PlayStatus::Playing,
            active_here: true,
            context_uri: context.map(Into::into),
            position: None,
            volume: 40,
            shuffle: false,
            repeat: Repeat::Off,
            actions: Actions::default(),
            liked: None,
            quality: None,
        }
    }

    /// The list as a page shows it; records what it returned.
    struct Shown {
        rows: Vec<TrackRow>,
        context: Option<String>,
        thumbs: bool,
        liked: HashMap<String, bool>,
        player: PlayerState,
        sel: Selection,
        scroll: Option<f32>,
        art: Option<Art>,
        actions: Vec<RowAction>,
        /// A button above the list, for egui's arrow-key focus moves to go to.
        above: bool,
        dir: tempfile::TempDir,
    }

    impl Shown {
        fn new(rows: Vec<TrackRow>) -> Shown {
            Shown {
                rows,
                context: Some(PLAYLIST.into()),
                thumbs: true,
                liked: HashMap::new(),
                player: player(None, None),
                sel: Selection::default(),
                scroll: None,
                art: None,
                actions: vec![],
                above: false,
                dir: tempfile::tempdir().unwrap(),
            }
        }
    }

    /// A harness `height` points tall with the window's real fonts, and whether they
    /// have [`GLYPHS`] (see [`test_support::builder`]).
    fn builder(height: f32, tofu: bool) -> (HarnessBuilder<Shown>, bool) {
        test_support::builder(vec2(900.0, height), tofu)
    }

    fn harness_with(builder: HarnessBuilder<Shown>, shown: Shown) -> Harness<'static, Shown> {
        builder.build_ui_state(
            |ui, s: &mut Shown| {
                if s.above {
                    let _ = ui.button("Above");
                }
                let art = s
                    .art
                    .get_or_insert_with(|| Art::with_dir(ui.ctx().clone(), s.dir.path().into()));
                let liked = |uri: &str| s.liked.get(uri).copied();
                let list = TrackList {
                    id_salt: "test",
                    rows: &s.rows,
                    context_uri: s.context.as_deref(),
                    show_thumbs: s.thumbs,
                    liked: &liked,
                    scroll: s.scroll.as_mut(),
                };
                if let Some(action) = list.show(ui, &s.player, &mut s.sel, art) {
                    s.actions.push(action);
                }
            },
            shown,
        )
    }

    fn harness(shown: Shown) -> (Harness<'static, Shown>, bool) {
        let (builder, glyphs) = builder(400.0, false);
        let mut harness = harness_with(builder, shown);
        harness.run();
        (harness, glyphs)
    }

    /// A point on the row labelled `label` (the `nth` such), in its title column: its
    /// centre is on the artists' links.
    fn on_title(harness: &Harness<'_, Shown>, label: &str, nth: usize) -> Pos2 {
        let rect = harness
            .query_all_by_label(label)
            .nth(nth)
            .unwrap_or_else(|| panic!("no {label} #{nth}"))
            .rect();
        pos2(rect.left() + 160.0, rect.center().y)
    }

    /// `clicks` clicks of `button` on a row's title column, in one frame: egui counts a
    /// double-click by time.
    fn click_row(
        harness: &mut Harness<'_, Shown>,
        label: &str,
        nth: usize,
        button: PointerButton,
        clicks: usize,
    ) {
        let pos = on_title(harness, label, nth);
        let events = &mut harness.input_mut().events;
        events.push(Event::PointerMoved(pos));
        for _ in 0..clicks {
            for pressed in [true, false] {
                events.push(Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers: Modifiers::NONE,
                });
            }
        }
        harness.run();
    }

    fn double_click(harness: &mut Harness<'_, Shown>, label: &str, nth: usize) {
        click_row(harness, label, nth, PointerButton::Primary, 2);
    }

    fn right_click(harness: &mut Harness<'_, Shown>, label: &str) {
        click_row(harness, label, 0, PointerButton::Secondary, 1);
    }

    fn take(harness: &mut Harness<'_, Shown>) -> Vec<RowAction> {
        std::mem::take(&mut harness.state_mut().actions)
    }

    #[test]
    fn duration_format() {
        assert_eq!(duration(230_526), "3:50");
        assert_eq!(duration(59_000), "0:59");
        assert_eq!(duration(0), "0:00");
        assert_eq!(duration(600_999), "10:00");
        assert_eq!(duration(4_000_000), "66:40");
    }

    #[test]
    fn playing_row_needs_same_context() {
        let r = row("spotify:track:a", None, "Alpha");
        let in_playlist = player(Some("spotify:track:a"), Some(PLAYLIST));
        assert!(is_playing_row(&r, Some(PLAYLIST), &in_playlist));
        // The same track from another list isn't this list's playing row.
        assert!(!is_playing_row(&r, Some("spotify:album:x"), &in_playlist));
        let alone = player(Some("spotify:track:a"), None);
        assert!(!is_playing_row(&r, Some(PLAYLIST), &alone));
        // A list with no context (search) marks the track whatever it plays in.
        assert!(is_playing_row(&r, None, &in_playlist));
        assert!(is_playing_row(&r, None, &alone));
        // Another track, or nothing.
        let other = player(Some("spotify:track:b"), Some(PLAYLIST));
        assert!(!is_playing_row(&r, Some(PLAYLIST), &other));
        assert!(!is_playing_row(&r, None, &other));
        assert!(!is_playing_row(&r, None, &player(None, Some(PLAYLIST))));
    }

    #[test]
    fn links_and_contexts() {
        assert_eq!(
            web_link("spotify:track:6rqhFgbbKwnb9MLmUQDhG6").as_deref(),
            Some("https://open.spotify.com/track/6rqhFgbbKwnb9MLmUQDhG6")
        );
        assert_eq!(
            web_link("spotify:episode:e1").as_deref(),
            Some("https://open.spotify.com/episode/e1")
        );
        assert_eq!(web_link("spotify:local:a:b:c:200"), None);
        assert_eq!(web_link("spotify:track:"), None);
        assert_eq!(web_link("nonsense"), None);
        assert!(is_page("spotify:artist:jr", "artist"));
        assert!(is_page("spotify:album:cd", "album"));
        assert!(!is_page("spotify:album:cd", "artist"));
        assert!(!is_page("spotify:artist:", "artist"));
        assert!(!is_page("spotify:artistx:jr", "artist"));
        assert!(!is_page("spotify:local:a:b:c:200", "album"));
        assert!(!is_page("", "artist"));
        assert_eq!(
            liked_context("dannycake"),
            "spotify:user:dannycake:collection"
        );
    }

    /// 10 000 rows in a 400 px tall list: only the dozen in view are laid out.
    #[test]
    fn only_visible_rows_laid_out() {
        let (harness, _) = harness(Shown::new(songs(10_000)));
        let laid_out = harness.query_all_by_label_contains("Song ").count();
        assert!((1..20).contains(&laid_out), "{laid_out} rows laid out");
        harness.get_by_label("Song 0");
        assert!(harness.query_by_label("Song 9999").is_none());
        // The rows don't overlap, and none is taller than the fixed height.
        let top = |label: &str| harness.get_by_label(label).rect().top();
        assert_eq!(top("Song 1") - top("Song 0"), theme::ROW_HEIGHT);
        assert_eq!(
            harness.get_by_label("Song 0").rect().height(),
            theme::ROW_HEIGHT
        );
    }

    #[test]
    fn double_click_plays_that_row() {
        let (mut harness, _) = harness(Shown::new(three()));
        // One click selects, and plays nothing.
        click_row(&mut harness, "Gamma", 0, PointerButton::Primary, 1);
        assert_eq!(harness.state().sel.index, Some(2));
        assert_eq!(take(&mut harness), vec![]);
        let toggled = |h: &Harness<'_, Shown>, label: &str| {
            h.get_by_label(label).accesskit_node().toggled() == Some(Toggled::True)
        };
        assert!(toggled(&harness, "Gamma"));
        assert!(!toggled(&harness, "Beta"));

        double_click(&mut harness, "Beta", 0);
        assert_eq!(take(&mut harness), vec![RowAction::Play { index: 1 }]);
        assert_eq!(harness.state().sel.index, Some(1));
    }

    /// Two copies of one track: the second plays as itself, told apart by its uid.
    #[test]
    fn duplicate_rows_play_by_uid() {
        let rows = vec![
            row("spotify:track:dup", Some("a"), "Twice"),
            row("spotify:track:dup", Some("b"), "Twice"),
        ];
        let (mut harness, _) = harness(Shown::new(rows.clone()));
        double_click(&mut harness, "Twice", 1);
        let actions = take(&mut harness);
        assert_eq!(actions, vec![RowAction::Play { index: 1 }]);
        let RowAction::Play { index } = actions[0] else {
            unreachable!()
        };
        assert_eq!(
            play_request(&rows, Some(PLAYLIST), index),
            Some(Request::PlayContext {
                context_uri: PLAYLIST.into(),
                track_uri: Some("spotify:track:dup".into()),
                track_uid: Some("b".into()),
                shuffle: None,
            })
        );
        assert_eq!(
            play_request(&rows, Some(PLAYLIST), 0),
            Some(Request::PlayContext {
                context_uri: PLAYLIST.into(),
                track_uri: Some("spotify:track:dup".into()),
                track_uid: Some("a".into()),
                shuffle: None,
            })
        );
        // Search results have no context: the track plays alone.
        assert_eq!(
            play_request(&rows, None, 1),
            Some(Request::PlayUri {
                uri: "spotify:track:dup".into()
            })
        );
        assert_eq!(play_request(&rows, Some(PLAYLIST), 2), None);
    }

    #[test]
    fn context_menu_labels() {
        let mut rows = three();
        rows[1].uri = "spotify:track:6rqhFgbbKwnb9MLmUQDhG6".into();
        let (mut harness, _) = harness(Shown::new(rows));
        right_click(&mut harness, "Beta");
        // Right-click selects the row too.
        assert_eq!(harness.state().sel.index, Some(1));
        let labels = [
            "Add to queue",
            "Start radio",
            "Go to artist",
            "Go to album",
            "Show credits",
            "Copy Spotify link",
        ];
        let tops: Vec<f32> = labels
            .iter()
            .map(|label| harness.get_by_label(label).rect().top())
            .collect();
        assert!(tops.is_sorted(), "{tops:?}");
        // Nothing else is in the menu.
        let buttons = harness
            .query_all_by_role(egui::accesskit::Role::Button)
            .filter(|b| b.accesskit_node().label().is_some_and(|l| l != "Like"))
            .count();
        assert_eq!(buttons, labels.len());

        harness.get_by_label("Copy Spotify link").click();
        harness.step();
        let link = "https://open.spotify.com/track/6rqhFgbbKwnb9MLmUQDhG6";
        assert!(
            harness
                .output()
                .platform_output
                .commands
                .contains(&OutputCommand::CopyText(link.into())),
            "{:?}",
            harness.output().platform_output.commands
        );
        harness.run();
        assert_eq!(take(&mut harness), vec![RowAction::CopyLink(link.into())]);
        // The menu closed.
        assert!(harness.query_by_label("Add to queue").is_none());

        // Each other entry acts on the right-clicked row.
        let cases = [
            (
                "Add to queue",
                RowAction::AddToQueue("spotify:track:c".into()),
            ),
            (
                "Start radio",
                RowAction::StartRadio("spotify:track:c".into()),
            ),
            (
                "Go to artist",
                RowAction::GoArtist("spotify:artist:jr".into()),
            ),
            ("Go to album", RowAction::GoAlbum("spotify:album:cd".into())),
            (
                "Show credits",
                RowAction::ShowCredits("spotify:track:c".into()),
            ),
        ];
        for (label, action) in cases {
            right_click(&mut harness, "Gamma");
            harness.get_by_label(label).click();
            harness.run();
            assert_eq!(take(&mut harness), vec![action], "{label}");
        }
    }

    /// A row with no album, no artist and no web page offers those entries disabled.
    #[test]
    fn context_menu_disables_what_a_row_lacks() {
        let mut rows = three();
        rows[0].uri = "spotify:local:a:b:c:200".into();
        rows[0].album = None;
        rows[0].artists = vec![];
        let (mut harness, _) = harness(Shown::new(rows));
        right_click(&mut harness, "Alpha");
        let disabled = |label: &str| harness.get_by_label(label).accesskit_node().is_disabled();
        assert!(disabled("Go to artist"));
        assert!(disabled("Go to album"));
        assert!(disabled("Copy Spotify link"));
        assert!(!disabled("Add to queue"));
    }

    /// Click focuses the list; ↑/↓ move the selection, scrolling it into view; Enter plays.
    #[test]
    fn keyboard_moves_and_plays() {
        let mut shown = Shown::new(songs(100));
        shown.above = true;
        let (mut harness, _) = harness(shown);
        // Before any click the list hasn't the focus: keys do nothing.
        harness.key_press(egui::Key::ArrowDown);
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(harness.state().sel, Selection::default());
        assert_eq!(take(&mut harness), vec![]);

        click_row(&mut harness, "Song 1", 0, PointerButton::Primary, 1);
        assert_eq!(
            harness.state().sel,
            Selection {
                index: Some(1),
                focused: true,
                hovered_link: None,
            }
        );
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert_eq!(harness.state().sel.index, Some(0));
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert_eq!(harness.state().sel.index, Some(0), "stops at the top");
        // ↑ at the top keeps the focus: it doesn't go to the button above.
        assert!(harness.state().sel.focused);
        assert!(!harness.get_by_label("Above").is_focused());
        for _ in 0..30 {
            harness.key_press(egui::Key::ArrowDown);
        }
        harness.run();
        assert_eq!(harness.state().sel.index, Some(30));
        assert!(harness.state().sel.focused);
        // Row 30 was scrolled into view.
        let list = harness.get_by_label(LIST_LABEL).rect();
        let row = harness.get_by_label("Song 30").rect();
        assert!(list.contains_rect(row), "{row:?} not in {list:?}");
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(take(&mut harness), vec![RowAction::Play { index: 30 }]);

        // Escape takes the focus away, and with it Enter.
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert!(!harness.state().sel.focused);
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(take(&mut harness), vec![]);
    }

    /// On the playing row its title and the ▶ for its number in the accent, and the
    /// accent stripe down its left edge; ♥ in the accent when liked, ♡ in the line grey
    /// when not or not known; the heart asks for the other state. The glyphs are a third
    /// bigger than the text (Look 2).
    #[test]
    fn playing_row_and_hearts() {
        let mut shown = Shown::new(three());
        shown.player = player(Some("spotify:track:b"), Some(PLAYLIST));
        shown.liked = HashMap::from([
            ("spotify:track:a".to_string(), true),
            ("spotify:track:b".to_string(), false),
        ]);
        let (mut harness, glyphs) = harness(shown);
        let texts = texts(&harness);
        let find = |text: &str| -> Vec<(Color32, Rect)> {
            texts
                .iter()
                .filter(|(t, _, _)| t == text)
                .map(|(_, c, r)| (*c, *r))
                .collect()
        };
        let beta = harness.get_by_label("Beta").rect();
        assert_eq!(find("Beta").len(), 1);
        assert_eq!(find("Beta")[0].0, theme::ACCENT);
        assert_eq!(find("Alpha")[0].0, theme::TEXT);
        // The playing row's number is ▶, in the accent; the others keep theirs.
        let play = find("▶");
        assert_eq!(play.len(), 1);
        assert_eq!(play[0].0, theme::ACCENT);
        assert!(beta.contains(play[0].1.center()));
        assert_eq!(size_of(&harness, "▶"), Some(theme::ICON_SMALL));
        assert_eq!(find("1").len(), 1);
        assert_eq!(find("2").len(), 0);
        assert_eq!(find("3").len(), 1);
        // Its secondary text stays secondary, as on every row.
        let artists = find("Jane Remover");
        assert_eq!(artists.len(), 3);
        assert!(artists.iter().all(|(c, _)| *c == theme::SECONDARY));
        let durations = find("3:50");
        assert_eq!(durations.len(), 3);
        assert!(durations.iter().all(|(c, _)| *c == theme::SECONDARY));
        // The 3 px stripe down the playing row's left edge, and no other accent fill.
        let stripes: Vec<Rect> = fills(&harness)
            .into_iter()
            .filter(|(_, c)| *c == theme::ACCENT)
            .map(|(r, _)| r)
            .collect();
        assert_eq!(stripes.len(), 1, "{stripes:?}");
        assert_eq!(stripes[0].width(), theme::STRIPE_WIDTH);
        assert_eq!(stripes[0].left(), beta.left());
        assert_eq!(stripes[0].y_range(), beta.y_range());

        // Hearts: liked, not liked, unknown.
        let hearts = find("♥");
        assert_eq!(hearts.len(), 1);
        assert_eq!(hearts[0].0, theme::ACCENT);
        let alpha = harness.get_by_label("Alpha").rect();
        assert!(alpha.contains(hearts[0].1.center()));
        let outlines = find("♡");
        assert_eq!(outlines.len(), 2);
        assert!(outlines.iter().all(|(c, _)| *c == theme::LINES));
        assert_eq!(size_of(&harness, "♥"), Some(theme::ICON_SMALL));
        assert_eq!(size_of(&harness, "♡"), Some(theme::ICON_SMALL));
        let liked: Vec<bool> = harness
            .query_all_by_label("Like")
            .map(|h| h.accesskit_node().toggled() == Some(Toggled::True))
            .collect();
        assert_eq!(liked, [true, false, false]);
        if glyphs {
            // The fonts the rows use have every glyph they draw.
            let font = TextStyle::Body.resolve(&harness.ctx.global_style());
            for c in GLYPHS {
                assert!(harness.ctx.fonts_mut(|f| f.has_glyph(&font, c)), "{c}");
            }
        }

        // A heart asks for the state it doesn't show (final review C1): the unknown one,
        // drawn as not liked, likes its track and never unlikes it; the liked one
        // unlikes. Neither selects anything.
        harness.query_all_by_label("Like").nth(2).unwrap().click();
        harness.run();
        assert_eq!(
            take(&mut harness),
            vec![RowAction::Heart {
                uri: "spotify:track:c".into(),
                liked: true
            }]
        );
        harness.query_all_by_label("Like").next().unwrap().click();
        harness.run();
        assert_eq!(
            take(&mut harness),
            vec![RowAction::Heart {
                uri: "spotify:track:a".into(),
                liked: false
            }]
        );
        assert_eq!(harness.state().sel.index, None);
    }

    /// A list with no context marks the track it plays whatever the context.
    #[test]
    fn search_rows_mark_the_track() {
        let mut shown = Shown::new(three());
        shown.context = None;
        shown.player = player(Some("spotify:track:c"), Some("spotify:album:x"));
        let (harness, _) = harness(shown);
        let texts = texts(&harness);
        let gamma = harness.get_by_label("Gamma").rect();
        let play: Vec<_> = texts.iter().filter(|(t, _, _)| t == "▶").collect();
        assert_eq!(play.len(), 1);
        assert!(gamma.contains(play[0].2.center()));
    }

    /// CJK and overlong titles keep the row height and stay on one line.
    #[test]
    fn rows_keep_their_height() {
        let long = "A Title Long Enough That No Column Could Ever Hold All Of It ".repeat(4);
        let rows = vec![
            row("spotify:track:a", None, "ジェーン・リムーバー"),
            row("spotify:track:b", None, &long),
            row("spotify:track:c", None, "Gamma"),
        ];
        let (builder, _) = builder(400.0, true);
        let mut harness = harness_with(builder, Shown::new(rows));
        harness.run();
        let cjk = harness.get_by_label("ジェーン・リムーバー").rect();
        let tall = harness.get_by_label(long.as_str()).rect();
        let gamma = harness.get_by_label("Gamma").rect();
        for r in [cjk, tall, gamma] {
            assert_eq!(r.height(), theme::ROW_HEIGHT);
        }
        assert_eq!(tall.top() - cjk.top(), theme::ROW_HEIGHT);
        assert_eq!(gamma.top() - tall.top(), theme::ROW_HEIGHT);
        // The long title is cut short with an ellipsis, inside its row.
        let shapes = &harness.output().shapes;
        let mut found = false;
        for clipped in shapes {
            let mut stack = vec![&clipped.shape];
            while let Some(shape) = stack.pop() {
                match shape {
                    Shape::Vec(v) => stack.extend(v.iter()),
                    Shape::Text(t) if t.galley.text() == long => {
                        found = true;
                        assert!(t.galley.elided);
                        assert_eq!(t.galley.rows.len(), 1);
                        let rect = t.galley.rect.translate(t.pos.to_vec2());
                        assert!(tall.contains_rect(rect), "{rect:?} past {tall:?}");
                    }
                    _ => {}
                }
            }
        }
        assert!(found, "the long title wasn't drawn");
    }

    /// The caller's offset is shown from, then kept up to date; changing it scrolls.
    #[test]
    fn scroll_offset_restored_and_written_back() {
        let mut shown = Shown::new(songs(1000));
        shown.scroll = Some(100.0 * theme::ROW_HEIGHT);
        let (builder, _) = builder(400.0, false);
        let mut harness = harness_with(builder.with_max_steps(64), shown);
        harness.run();
        harness.get_by_label("Song 100");
        assert!(harness.query_by_label("Song 0").is_none());
        assert_eq!(harness.state().scroll, Some(4000.0));

        // The owner scrolls down: the caller's copy follows.
        let pos = on_title(&harness, "Song 101", 0);
        harness.event(Event::PointerMoved(pos));
        harness.event(Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta: vec2(0.0, -200.0),
            modifiers: Modifiers::NONE,
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
        });
        harness.run();
        let after = harness.state().scroll.unwrap();
        assert!(after > 4000.0, "{after}");
        assert!(harness.query_by_label("Song 100").is_none());

        // The caller jumps to the top.
        harness.state_mut().scroll = Some(0.0);
        harness.run();
        harness.get_by_label("Song 0");
        assert_eq!(harness.state().scroll, Some(0.0));
    }

    /// Rows that went away (a track unliked off Liked Songs) don't leave the selection
    /// past the end.
    #[test]
    fn selection_stays_in_the_list() {
        let mut shown = Shown::new(three());
        shown.sel.index = Some(7);
        let (mut harness, _) = harness(shown);
        assert_eq!(harness.state().sel.index, Some(2));
        harness.state_mut().rows.clear();
        harness.run();
        assert_eq!(harness.state().sel.index, None);
    }

    /// Every line segment drawn last frame.
    fn lines(harness: &Harness<'_, Shown>) -> Vec<([Pos2; 2], Stroke)> {
        fn walk(shape: &Shape, out: &mut Vec<([Pos2; 2], Stroke)>) {
            match shape {
                Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                Shape::LineSegment { points, stroke } => out.push((*points, *stroke)),
                _ => {}
            }
        }
        let mut out = vec![];
        for clipped in &harness.output().shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    /// The white underlines drawn last frame.
    fn underlines(harness: &Harness<'_, Shown>) -> Vec<[Pos2; 2]> {
        lines(harness)
            .into_iter()
            .filter(|(_, stroke)| *stroke == Stroke::new(1.0, theme::TEXT))
            .map(|(points, _)| points)
            .collect()
    }

    fn link_rect(harness: &Harness<'_, Shown>, label: &str, nth: usize) -> Rect {
        harness
            .query_all_by_role_and_label(Role::Link, label)
            .nth(nth)
            .unwrap_or_else(|| panic!("no link {label} #{nth}"))
            .rect()
    }

    /// The colour `text` was drawn in inside `rect`, last frame.
    fn colour(harness: &Harness<'_, Shown>, text: &str, rect: Rect) -> Option<Color32> {
        texts(harness)
            .into_iter()
            .find(|(t, _, r)| t == text && rect.contains(r.center()))
            .map(|(_, c, _)| c)
    }

    /// Artist and album names are links: white and underlined under the pointer, which
    /// becomes a hand; a click opens the page and leaves the selection; the caller learns
    /// which link is hovered.
    #[test]
    fn artist_and_album_links() {
        let mut rows = three();
        // A nameless artist between two is skipped, not the end of the list.
        rows[1].artists.push(Named {
            uri: "spotify:artist:none".into(),
            name: String::new(),
        });
        rows[1].artists.push(Named {
            uri: "spotify:artist:dz".into(),
            name: "Dltzk".into(),
        });
        let mut shown = Shown::new(rows);
        shown.sel.index = Some(0);
        let (mut harness, _) = harness(shown);
        // Each artist is a link of its own, ", " between them.
        assert_eq!(
            harness
                .query_all_by_role_and_label(Role::Link, "Jane Remover")
                .count(),
            3
        );
        let jane = link_rect(&harness, "Jane Remover", 1);
        let dltzk = link_rect(&harness, "Dltzk", 0);
        let comma: Vec<Rect> = texts(&harness)
            .into_iter()
            .filter(|(t, c, _)| t == ", " && *c == theme::SECONDARY)
            .map(|(_, _, r)| r)
            .collect();
        assert_eq!(comma.len(), 1);
        assert!(jane.right() <= comma[0].left() && comma[0].right() <= dltzk.left());
        assert!(underlines(&harness).is_empty());
        assert_eq!(harness.state().sel.hovered_link, None);

        harness
            .query_all_by_role_and_label(Role::Link, "Dltzk")
            .next()
            .unwrap()
            .hover();
        harness.run();
        assert_eq!(colour(&harness, "Dltzk", dltzk), Some(theme::TEXT));
        assert_eq!(
            colour(&harness, "Jane Remover", jane),
            Some(theme::SECONDARY)
        );
        let under = underlines(&harness);
        assert_eq!(under.len(), 1, "{under:?}");
        let [from, to] = under[0];
        assert_eq!(from.y, to.y);
        assert_eq!((from.x, to.x), (dltzk.left(), dltzk.right()));
        assert!(
            dltzk.y_range().contains(from.y),
            "{from:?} not under {dltzk:?}"
        );
        assert_eq!(
            harness.output().platform_output.cursor_icon,
            CursorIcon::PointingHand
        );
        assert_eq!(
            harness.state().sel.hovered_link,
            Some(LinkTarget::Artist("spotify:artist:dz".into()))
        );

        harness
            .query_all_by_role_and_label(Role::Link, "Dltzk")
            .next()
            .unwrap()
            .click();
        harness.run();
        assert_eq!(
            take(&mut harness),
            vec![RowAction::GoArtist("spotify:artist:dz".into())]
        );
        assert_eq!(harness.state().sel.index, Some(0), "the selection stays");

        // The album, likewise.
        let album = link_rect(&harness, "Census Designated", 2);
        harness
            .query_all_by_role_and_label(Role::Link, "Census Designated")
            .nth(2)
            .unwrap()
            .hover();
        harness.run();
        assert_eq!(
            colour(&harness, "Census Designated", album),
            Some(theme::TEXT)
        );
        let under = underlines(&harness);
        assert_eq!(under.len(), 1, "{under:?}");
        assert_eq!(
            (under[0][0].x, under[0][1].x),
            (album.left(), album.right())
        );
        assert_eq!(
            harness.state().sel.hovered_link,
            Some(LinkTarget::Album("spotify:album:cd".into()))
        );
        harness
            .query_all_by_role_and_label(Role::Link, "Census Designated")
            .nth(2)
            .unwrap()
            .click();
        harness.run();
        assert_eq!(
            take(&mut harness),
            vec![RowAction::GoAlbum("spotify:album:cd".into())]
        );
        assert_eq!(harness.state().sel.index, Some(0));

        // Off the links: nothing hovered, nothing underlined, the usual pointer.
        harness.event(Event::PointerMoved(on_title(&harness, "Beta", 0)));
        harness.run();
        assert_eq!(harness.state().sel.hovered_link, None);
        assert!(underlines(&harness).is_empty());
        assert_eq!(
            harness.output().platform_output.cursor_icon,
            CursorIcon::Default
        );

        // A right-click on a link offers the row's menu, and selects the row.
        harness
            .query_all_by_role_and_label(Role::Link, "Jane Remover")
            .nth(2)
            .unwrap()
            .click_secondary();
        harness.run();
        assert_eq!(harness.state().sel.index, Some(2));
        harness.get_by_label("Show credits").click();
        harness.run();
        assert_eq!(
            take(&mut harness),
            vec![RowAction::ShowCredits("spotify:track:c".into())]
        );
    }

    /// A local file's artist and album have no page: their names show, but aren't links.
    #[test]
    fn local_rows_have_no_links() {
        let mut local = row(
            "spotify:local:SINN6R:2Often:Interview:90",
            None,
            "Interview",
        );
        local.artists = vec![Named {
            uri: String::new(),
            name: "SINN6R".into(),
        }];
        local.album = Some(Named {
            uri: String::new(),
            name: "2Often".into(),
        });
        let (mut harness, _) = harness(Shown::new(vec![local]));
        assert_eq!(harness.query_all_by_role(Role::Link).count(), 0);
        let drawn = texts(&harness);
        let artist = drawn
            .iter()
            .find(|(t, _, _)| t == "SINN6R")
            .map(|(_, c, r)| (*c, *r))
            .expect("the artist's name is drawn");
        assert_eq!(artist.0, theme::SECONDARY);
        assert!(drawn.iter().any(|(t, _, _)| t == "2Often"));

        harness.event(Event::PointerMoved(artist.1.center()));
        harness.run();
        assert_eq!(harness.state().sel.hovered_link, None);
        assert!(underlines(&harness).is_empty());
        assert_eq!(colour(&harness, "SINN6R", artist.1), Some(theme::SECONDARY));
        // Nor does the menu offer to go there.
        right_click(&mut harness, "Interview");
        let disabled = |label: &str| harness.get_by_label(label).accesskit_node().is_disabled();
        assert!(disabled("Go to artist"));
        assert!(disabled("Go to album"));
    }
}
