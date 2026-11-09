//! The right panel, toggled from the bottom bar, with three tabs: Queue, Lyrics and
//! Credits. Which is open is the saved view's (`ViewState::panel`, `None` while closed).
//!
//! - **Queue:** what plays next, as a track list with covers; the rows' hearts, links
//!   and menu work as on a page, but a double-click (or Enter) plays nothing.
//! - **Lyrics:** the current track's. Synced ones follow the bottom bar's position: the
//!   current line in the accent, the rest secondary, kept centred unless the owner
//!   scrolled them in the last [`OWNER_HOLD`]; a click on a line seeks to its start. While
//!   playing, a repaint comes when the next line starts. Unsynced lyrics are plain text.
//! - **Credits:** the track `Show credits` picked, until the tab is left; else the
//!   current track's. Each role, with the names under it.
//!
//! Lyrics and credits are asked for once per track and kept for the session. The link
//! doesn't queue while disconnected: an answer the connection's loss swallowed is asked
//! for again once connected. A failure stays in its tab, with Retry.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, CursorIcon, FontId, Galley, Label, Rect, Response, RichText, Role, ScrollArea,
    Sense, Stroke, TextStyle, TextWrapMode, Ui, Vec2, WidgetInfo, WidgetText, vec2,
};
use engine_ipc::{PlayerState, Reply, Request};
use log::{debug, warn};
use playback::event::NowPlaying;
use spot_core::pages::{CreditRole, Image, LyricLine, Lyrics, Named, TrackRow};

use crate::{
    art::Art,
    bottom_bar::BottomBar,
    link::{RequestId, Requests},
    notices,
    page_frame::{self, LOADING, RETRY},
    pages::PageViews,
    theme,
    track_list::{RowAction, Selection, TrackList, is_page},
    view_state::{PanelTab, ViewState},
};

/// The panel's width, its margins included, in points.
pub const WIDTH: f32 = 360.0;
/// The tabs, in order.
pub const QUEUE: &str = "Queue";
pub const LYRICS: &str = "Lyrics";
pub const CREDITS: &str = "Credits";
/// What a track without lyrics, or without credits, shows.
pub const NO_LYRICS: &str = "No lyrics";
pub const NO_CREDITS: &str = "No credits";
/// What an empty queue shows.
pub const NOTHING_QUEUED: &str = "Nothing queued";
/// What a reply of another kind than the one asked for says.
pub const OTHER_REPLY: &str = "The engine sent something other than what was asked for.";
/// How long after the owner scrolls the lyrics they are left where the owner put them.
pub const OWNER_HOLD: Duration = Duration::from_secs(5);
/// The longest the highlight waits for a repaint while playing: the bar's own clock.
const LINE_WAIT_MAX: Duration = Duration::from_secs(1);
/// The lyrics' size, in points, and the space between their lines.
const LYRIC_SIZE: f32 = 18.0;
const LINE_GAP: f32 = 10.0;

const TABS: [(PanelTab, &str); 3] = [
    (PanelTab::Queue, QUEUE),
    (PanelTab::Lyrics, LYRICS),
    (PanelTab::Credits, CREDITS),
];

/// The line playing at `pos_ms`: the last to start at or before it; `None` before the
/// first.
pub fn current_line(lines: &[LyricLine], pos_ms: u64) -> Option<usize> {
    lines.iter().rposition(|line| line.start_ms <= pos_ms)
}

/// How long from `pos_ms` until the next line starts, at most [`LINE_WAIT_MAX`]: when to
/// repaint for the highlight to move on time. `None` once the last line has started.
pub fn line_wait(lines: &[LyricLine], pos_ms: u64) -> Option<Duration> {
    let next = lines
        .iter()
        .map(|line| line.start_ms)
        .filter(|&start| start > pos_ms)
        .min()?;
    Some(Duration::from_millis(next - pos_ms).min(LINE_WAIT_MAX))
}

/// What the window knows of a track's lyrics or its credits.
#[derive(Debug, Clone, PartialEq)]
enum Fetch<T> {
    /// Asked for, or to be once connected.
    Loading,
    Ready(T),
    /// Why there are none, in the engine's words.
    Failed(String),
}

/// Lyrics or credits, by track uri, for the session; and the requests out for them.
#[derive(Debug)]
struct ByTrack<T> {
    states: HashMap<String, Fetch<T>>,
    /// Each request out, and the track it was for.
    pending: HashMap<RequestId, String>,
    /// The request for a track's.
    ask: fn(String) -> Request,
}

impl<T> ByTrack<T> {
    fn new(ask: fn(String) -> Request) -> ByTrack<T> {
        ByTrack {
            states: HashMap::new(),
            pending: HashMap::new(),
            ask,
        }
    }

    fn get(&self, uri: &str) -> Option<&Fetch<T>> {
        self.states.get(uri)
    }

    /// Asks for `uri`'s once: Loading until they come. While disconnected nothing is
    /// sent; they are asked for once connected.
    fn ensure(&mut self, link: &dyn Requests, uri: &str, connected: bool) {
        if !self.states.contains_key(uri) {
            self.load(link, uri, connected);
        }
    }

    /// Asks for `uri`'s (again: Retry), Loading meanwhile.
    fn load(&mut self, link: &dyn Requests, uri: &str, connected: bool) {
        self.states.insert(uri.to_owned(), Fetch::Loading);
        if connected {
            let id = link.send((self.ask)(uri.to_owned()));
            self.pending.insert(id, uri.to_owned());
        }
    }

    fn wants(&self, id: RequestId) -> bool {
        self.pending.contains_key(&id)
    }

    /// The answer to request `id`, made a `T` (or why not) by the caller.
    fn on_reply(&mut self, id: RequestId, result: Result<T, String>) {
        let Some(uri) = self.pending.remove(&id) else {
            return;
        };
        match result {
            Ok(value) => {
                self.states.insert(uri, Fetch::Ready(value));
            }
            // Not the track's failure: it stays Loading, asked for again once connected.
            Err(e) if page_frame::is_connection_error(&e) => {
                debug!("{uri}: {e}; asking again once connected");
            }
            Err(e) => {
                self.states.insert(uri, Fetch::Failed(e));
            }
        }
    }

    /// The link (re)connected: answers still out are lost, and what was waiting or
    /// failed is asked for again when next shown. What came is kept.
    fn on_connected(&mut self) {
        self.pending.clear();
        self.states
            .retain(|_, state| matches!(state, Fetch::Ready(_)));
    }
}

/// `result` as lyrics: `None` for a track without.
fn lyrics_of(result: Result<Reply, String>) -> Result<Option<Lyrics>, String> {
    match result? {
        Reply::Lyrics(lyrics) => Ok(lyrics),
        other => {
            warn!("the engine answered Lyrics with {other:?}");
            Err(OTHER_REPLY.into())
        }
    }
}

/// `result` as credits: empty for a track without.
fn credits_of(result: Result<Reply, String>) -> Result<Vec<CreditRole>, String> {
    match result? {
        Reply::Credits(roles) => Ok(roles),
        other => {
            warn!("the engine answered Credits with {other:?}");
            Err(OTHER_REPLY.into())
        }
    }
}

/// The auto-scroll: centres each new current line, but leaves the lyrics where the owner
/// scrolled them for [`OWNER_HOLD`].
#[derive(Debug, Default)]
struct Follow {
    /// The line last centred: its track's uri and its index.
    centred: Option<(String, usize)>,
    /// When the owner last scrolled the lyrics.
    scrolled_at: Option<Instant>,
}

impl Follow {
    /// The owner scrolled the lyrics: they stay put for [`OWNER_HOLD`], then the current
    /// line is centred again.
    fn owner_scrolled(&mut self, now: Instant) {
        self.scrolled_at = Some(now);
        self.centred = None;
    }

    /// Whether to centre line `line` of `uri`'s lyrics now: it isn't yet, and the owner
    /// hasn't scrolled them within [`OWNER_HOLD`]. True once per line.
    fn centre(&mut self, uri: &str, line: usize, now: Instant) -> bool {
        let held = self
            .scrolled_at
            .is_some_and(|at| now.saturating_duration_since(at) < OWNER_HOLD);
        let done = self
            .centred
            .as_ref()
            .is_some_and(|(u, l)| u == uri && *l == line);
        if held || done {
            return false;
        }
        self.centred = Some((uri.to_owned(), line));
        true
    }
}

/// What the panel keeps between frames.
#[derive(Debug)]
pub struct SidePanel {
    lyrics: ByTrack<Option<Lyrics>>,
    credits: ByTrack<Vec<CreditRole>>,
    /// The queue, as the track list draws it.
    queue: Vec<TrackRow>,
    /// Whether the queue's hearts have been asked about since it last changed.
    queue_asked: bool,
    selection: Selection,
    follow: Follow,
    connected: bool,
    /// The tab shown when the panel last closed, which the toggle opens it on.
    last: Option<PanelTab>,
}

impl Default for SidePanel {
    fn default() -> SidePanel {
        SidePanel {
            lyrics: ByTrack::new(|track_uri| Request::Lyrics { track_uri }),
            credits: ByTrack::new(|track_uri| Request::Credits { track_uri }),
            queue: Vec::new(),
            queue_asked: false,
            selection: Selection::default(),
            follow: Follow::default(),
            connected: false,
            last: None,
        }
    }
}

impl SidePanel {
    /// The engine's queue changed (or came with a connection).
    pub fn set_queue(&mut self, queue: &[NowPlaying]) {
        self.queue = queue.iter().map(queue_row).collect();
        self.queue_asked = false;
    }

    /// The link (re)connected: what was asked before is asked again, as needed.
    pub fn on_connected(&mut self) {
        self.connected = true;
        self.lyrics.on_connected();
        self.credits.on_connected();
        self.queue_asked = false;
    }

    pub fn on_disconnected(&mut self) {
        self.connected = false;
    }

    /// `Event::LikedSetReady`: the queue's hearts are asked about again.
    pub fn on_liked_set_ready(&mut self) {
        self.queue_asked = false;
    }

    /// Whether the reply to request `id` is the panel's: lyrics or credits. A queue row's
    /// requests are the pages' (`PageViews`).
    pub fn wants(&self, id: RequestId) -> bool {
        self.lyrics.wants(id) || self.credits.wants(id)
    }

    /// The reply to request `id`, which [`SidePanel::wants`].
    pub fn on_reply(&mut self, id: RequestId, result: Result<Reply, String>) {
        if self.lyrics.wants(id) {
            self.lyrics.on_reply(id, lyrics_of(result));
        } else if self.credits.wants(id) {
            self.credits.on_reply(id, credits_of(result));
        }
    }

    /// The bottom bar's toggle: closes the panel, or opens it on the tab it last showed
    /// (the queue, the first time).
    pub fn toggle(&mut self, tab: &mut Option<PanelTab>) {
        match tab.take() {
            Some(shown) => self.last = Some(shown),
            None => *tab = Some(self.last.unwrap_or(PanelTab::Queue)),
        }
    }
}

/// A queued track as a track row: its cover as Art takes it (ruling R2: one URL).
fn queue_row(track: &NowPlaying) -> TrackRow {
    let artists = track
        .artists
        .iter()
        .enumerate()
        .map(|(i, name)| Named {
            uri: track.artist_uris.get(i).cloned().unwrap_or_default(),
            name: name.clone(),
        })
        .collect();
    let album = track.album.as_ref().map(|name| Named {
        uri: track.album_uri.clone().unwrap_or_default(),
        name: name.clone(),
    });
    let images = track
        .cover_url
        .iter()
        .map(|url| Image {
            url: url.clone(),
            width: None,
        })
        .collect();
    TrackRow {
        uri: track.uri.clone(),
        uid: None,
        title: track.title.clone(),
        artists,
        album,
        duration_ms: track.duration_ms.unwrap_or(0),
        images,
        added_at: None,
    }
}

/// What the panel draws with, and what it may change, for a frame.
pub struct PanelCx<'a> {
    pub link: &'a dyn Requests,
    /// The saved view: the panel's tab, and the history a queue row's link adds to.
    pub view: &'a mut ViewState,
    pub art: &'a mut Art,
    pub player: &'a PlayerState,
    /// Whether the player takes commands (Soloist ready): only then does a line seek.
    pub ready: bool,
    /// The bottom bar: the position the lyrics follow, and the seek a line makes.
    pub bar: &'a mut BottomBar,
    /// The pages' state: the hearts, the track `Show credits` picked (dropped once the
    /// Credits tab is left), and the requests a queue row sends.
    pub views: &'a mut PageViews,
    pub now: Instant,
    /// Unix milliseconds, for the position.
    pub now_ms: i64,
}

/// The panel down the right of `ui`, while open: add it after the bottom bar and before
/// the central panel. Call it on every frame, open or not.
pub fn show(ui: &mut Ui, panel: &mut SidePanel, mut cx: PanelCx<'_>) {
    if cx.view.panel != Some(PanelTab::Credits) {
        cx.views.credits = None;
    }
    if cx.view.panel != Some(PanelTab::Lyrics) {
        // Back on the lyrics, the current line is centred again.
        panel.follow.centred = None;
    }
    let Some(tab) = cx.view.panel else {
        return;
    };
    egui::Panel::right("side-panel")
        .resizable(false)
        .exact_size(WIDTH)
        .frame(
            egui::Frame::new()
                .fill(theme::SURFACE)
                .inner_margin(egui::Margin::same(12)),
        )
        .show(ui, |ui| {
            if let Some(picked) = tabs(ui, tab) {
                cx.view.panel = Some(picked);
            }
            ui.add_space(12.0);
            match tab {
                PanelTab::Queue => queue(ui, panel, &mut cx),
                PanelTab::Lyrics => lyrics(ui, panel, &mut cx),
                PanelTab::Credits => credits(ui, panel, &mut cx),
            }
            // Another tab picked, or a queue row's `Show credits`: drawn on the next frame.
            if cx.view.panel != Some(tab) {
                ui.ctx().request_repaint();
            }
        });
}

/// The tabs along the panel's top: the one shown white over a thin line in the accent,
/// the others secondary (white under the pointer). The one clicked, if another.
fn tabs(ui: &mut Ui, shown: PanelTab) -> Option<PanelTab> {
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 20.0;
        for (tab, label) in TABS {
            let galley = WidgetText::from(label).into_galley(
                ui,
                Some(TextWrapMode::Extend),
                f32::INFINITY,
                TextStyle::Button,
            );
            let size = galley.size() + vec2(0.0, 6.0);
            let (rect, response) = ui.allocate_exact_size(size, Sense::click());
            let selected = tab == shown;
            response.widget_info(|| WidgetInfo::selected(Role::Tab, true, selected, label));
            let hovered = response.hovered();
            if ui.is_rect_visible(rect) {
                let colour = match (selected, hovered) {
                    (true, _) | (false, true) => theme::TEXT,
                    (false, false) => theme::SECONDARY,
                };
                let painter = ui.painter();
                painter.galley(rect.min, galley, colour);
                if selected {
                    let line = Stroke::new(1.0, theme::ACCENT);
                    painter.hline(rect.x_range(), rect.bottom() - 0.5, line);
                }
            }
            if hovered {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if response.clicked() && !selected {
                picked = Some(tab);
            }
        }
    });
    picked
}

/// `text`, secondary: a tab with nothing to show.
fn dim(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(theme::SECONDARY));
}

/// A failure in its tab: `⚠` and the engine's words, then Retry, in the error's red box;
/// whether Retry was clicked.
fn failed(ui: &mut Ui, message: &str) -> bool {
    notices::error_box(ui, |ui| {
        ui.horizontal_top(|ui| {
            page_frame::warning_icon(ui);
            ui.add(Label::new(message).wrap());
        });
        theme::main_button(ui, RETRY).clicked()
    })
}

/// The queue: a track list whose double-click plays nothing. Its hearts are asked about
/// once per change of the queue, for the tracks the engine hasn't answered for as it is
/// now (since the connection, and since it read Liked Songs).
fn queue(ui: &mut Ui, panel: &mut SidePanel, cx: &mut PanelCx) {
    let SidePanel {
        queue,
        queue_asked,
        selection,
        connected,
        ..
    } = panel;
    if queue.is_empty() {
        return dim(ui, NOTHING_QUEUED);
    }
    if !*queue_asked && *connected {
        let mut seen = HashSet::new();
        let unknown: Vec<String> = queue
            .iter()
            .map(|row| row.uri.as_str())
            .filter(|uri| seen.insert(*uri) && !cx.views.liked_set.is_current(uri))
            .map(str::to_owned)
            .collect();
        cx.views.liked_set.ask_uris(cx.link, unknown);
        *queue_asked = true;
    }
    let liked_set = &cx.views.liked_set;
    let liked = |uri: &str| liked_set.get(uri);
    let list = TrackList {
        id_salt: "queue",
        rows: queue,
        context_uri: None,
        show_thumbs: true,
        liked: &liked,
        scroll: None,
    };
    match list.show(ui, cx.player, selection, cx.art) {
        // The queue isn't a list to play from: a double-click, or Enter, does nothing.
        None | Some(RowAction::Play { .. }) => {}
        Some(action) => {
            let view = &mut *cx.view;
            cx.views
                .row_action(action, cx.link, &mut view.nav, &mut view.panel);
        }
    }
}

/// The current track's lyrics.
fn lyrics(ui: &mut Ui, panel: &mut SidePanel, cx: &mut PanelCx) {
    let player = cx.player;
    // Only a track has lyrics: a local file or an episode isn't asked about.
    let track = player.track.as_ref().map(|t| t.uri.as_str());
    let Some(uri) = track.filter(|uri| is_page(uri, "track")) else {
        return dim(ui, NO_LYRICS);
    };
    let SidePanel {
        lyrics,
        follow,
        connected,
        ..
    } = panel;
    lyrics.ensure(cx.link, uri, *connected);
    let mut retry = false;
    match lyrics.get(uri) {
        None | Some(Fetch::Loading) => dim(ui, LOADING),
        Some(Fetch::Failed(message)) => retry = failed(ui, message),
        Some(Fetch::Ready(Some(words))) if words.lines.iter().any(has_words) => {
            if words.synced {
                synced(ui, words, uri, follow, cx);
            } else {
                plain(ui, words, uri);
            }
        }
        // Carry 2: none, or only empty lines.
        Some(Fetch::Ready(_)) => dim(ui, NO_LYRICS),
    }
    if retry {
        lyrics.load(cx.link, uri, *connected);
    }
}

fn has_words(line: &LyricLine) -> bool {
    !line.words.trim().is_empty()
}

/// `words` in the lyrics' size, wrapped to `width`, in the colour it is painted with.
fn lyric_galley(ui: &Ui, words: &str, width: f32) -> Arc<Galley> {
    WidgetText::from(RichText::new(words).font(FontId::proportional(LYRIC_SIZE))).into_galley(
        ui,
        Some(TextWrapMode::Wrap),
        width,
        TextStyle::Body,
    )
}

/// Synced lyrics, following the bar's position: the current line white and centred
/// (unless the owner scrolled), a click on a line seeking to it.
fn synced(ui: &mut Ui, lyrics: &Lyrics, uri: &str, follow: &mut Follow, cx: &mut PanelCx) {
    let (position, playing) = cx.bar.shown_position(cx.player, cx.now, cx.now_ms);
    let current = position.and_then(|pos| current_line(&lyrics.lines, pos));
    let seeks = cx.ready && cx.player.actions.seek;
    // Before the scroll area takes this frame's wheel.
    if owner_scrolled(ui, ui.available_rect_before_wrap()) {
        follow.owner_scrolled(cx.now);
    }
    let now = cx.now;
    let mut seek = None;
    ScrollArea::vertical()
        .id_salt(("lyrics", uri))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = LINE_GAP;
            for (i, line) in lyrics.lines.iter().enumerate() {
                // An instrumental break: nothing to show or click.
                if !has_words(line) {
                    continue;
                }
                let is_current = current == Some(i);
                let response = lyric_line(ui, &line.words, is_current, seeks);
                if response.clicked() {
                    seek = Some(line.start_ms);
                }
                if is_current && follow.centre(uri, i, now) {
                    ui.scroll_to_rect(response.rect, Some(Align::Center));
                }
            }
        });
    if let Some(position_ms) = seek {
        cx.bar.seek_to(cx.link, cx.player, position_ms, cx.now_ms);
        ui.ctx().request_repaint();
    }
    // Carry 3: the highlight moves when the next line starts, not at the bar's next tick.
    if playing && let Some(wait) = position.and_then(|pos| line_wait(&lyrics.lines, pos)) {
        ui.ctx().request_repaint_after(wait);
    }
}

/// Whether the owner scrolled `area` this frame: the wheel or a touchpad over it, or a
/// drag begun in it (its scroll bar's, or the content's).
fn owner_scrolled(ui: &Ui, area: Rect) -> bool {
    ui.input(|i| {
        let over = i.pointer.hover_pos().is_some_and(|pos| area.contains(pos));
        let wheel = i.smooth_scroll_delta != Vec2::ZERO;
        let dragged = i.pointer.is_decidedly_dragging()
            && i.pointer
                .press_origin()
                .is_some_and(|pos| area.contains(pos));
        (over && wheel) || dragged
    })
}

/// One synced line, wrapped to the panel's width: in the accent when `current`, else
/// secondary (white under the pointer when it seeks).
fn lyric_line(ui: &mut Ui, words: &str, current: bool, seeks: bool) -> Response {
    let width = ui.available_width();
    let galley = lyric_galley(ui, words, width);
    let sense = if seeks {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(vec2(width, galley.size().y), sense);
    response.widget_info(|| WidgetInfo::selected(Role::ListItem, seeks, current, words));
    let hovered = seeks && response.hovered();
    if ui.is_rect_visible(rect) {
        let colour = match (current, hovered) {
            (true, _) => theme::ACCENT,
            (false, true) => theme::TEXT,
            (false, false) => theme::SECONDARY,
        };
        ui.painter().galley(rect.min, galley, colour);
    }
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

/// Unsynced lyrics: plain text, white.
fn plain(ui: &mut Ui, lyrics: &Lyrics, uri: &str) {
    ScrollArea::vertical()
        .id_salt(("lyrics", uri))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = LINE_GAP;
            let width = ui.available_width();
            for line in lyrics.lines.iter().filter(|line| has_words(line)) {
                let galley = lyric_galley(ui, &line.words, width);
                let size = vec2(width, galley.size().y);
                let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
                let words = line.words.as_str();
                response.widget_info(|| WidgetInfo::labeled(Role::Label, true, words));
                if ui.is_rect_visible(rect) {
                    ui.painter().galley(rect.min, galley, theme::TEXT);
                }
            }
        });
}

/// The credits of the track `Show credits` picked, else the current track's.
fn credits(ui: &mut Ui, panel: &mut SidePanel, cx: &mut PanelCx) {
    let player = cx.player;
    let current = player.track.as_ref().map(|t| t.uri.as_str());
    let picked = cx.views.credits.as_deref().or(current);
    // Only a track has credits: a local file or an episode isn't asked about.
    let Some(uri) = picked.filter(|uri| is_page(uri, "track")) else {
        return dim(ui, NO_CREDITS);
    };
    let SidePanel {
        credits, connected, ..
    } = panel;
    credits.ensure(cx.link, uri, *connected);
    let mut retry = false;
    match credits.get(uri) {
        None | Some(Fetch::Loading) => dim(ui, LOADING),
        Some(Fetch::Failed(message)) => retry = failed(ui, message),
        Some(Fetch::Ready(roles)) => {
            let named = |role: &&CreditRole| {
                !role.role.trim().is_empty() && role.names.iter().any(|n| !n.trim().is_empty())
            };
            let mut shown = roles.iter().filter(named).peekable();
            if shown.peek().is_none() {
                dim(ui, NO_CREDITS);
            } else {
                ScrollArea::vertical()
                    .id_salt(("credits", uri))
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        for role in shown {
                            ui.label(RichText::new(&role.role).color(theme::TEXT));
                            for name in role.names.iter().filter(|n| !n.trim().is_empty()) {
                                ui.label(RichText::new(name).color(theme::SECONDARY));
                            }
                            ui.add_space(12.0);
                        }
                    });
            }
        }
    }
    if retry {
        credits.load(cx.link, uri, *connected);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use eframe::egui::{
        Color32, Event, Modifiers, MouseWheelSource, MouseWheelUnit, PointerButton, Pos2,
        TouchPhase, ViewportId, pos2, vec2,
    };
    use egui_kittest::{Harness, kittest::Queryable};
    use engine_ipc::IpcError;
    use playback::{
        command::Repeat,
        event::{Actions, PlayStatus, Position},
    };

    use super::*;
    use crate::{
        test_support::{self, FakeLink, texts},
        track_list::{HEART_LABEL, SHOW_CREDITS},
    };

    /// The engine's clock at the start of a test, in Unix milliseconds.
    const T0_MS: i64 = 1_800_000_000_000;
    const MS: Duration = Duration::from_millis(1);
    const NERVE: &str = "spotify:track:nerve";
    const OTHER: &str = "spotify:track:other";

    fn line(start_ms: u64, words: &str) -> LyricLine {
        LyricLine {
            start_ms,
            words: words.into(),
        }
    }

    /// Lines at 2850 and 6000 ms.
    fn two_lines() -> Vec<LyricLine> {
        vec![line(2850, "first line"), line(6000, "second line")]
    }

    fn lyrics(synced: bool, lines: Vec<LyricLine>) -> Lyrics {
        Lyrics {
            synced,
            provider: "MusixMatch".into(),
            lines,
        }
    }

    fn track(uri: &str, title: &str) -> NowPlaying {
        NowPlaying {
            uri: uri.into(),
            title: title.into(),
            artists: vec!["Jane Remover".into()],
            artist_uris: vec!["spotify:artist:jane".into()],
            album: Some("Census Designated".into()),
            album_uri: Some("spotify:album:census".into()),
            duration_ms: Some(150_000),
            cover_url: Some(format!("https://i.scdn.co/image/{title}")),
        }
    }

    /// Playing `nerve` here, `at_ms` in as of `T0_MS`.
    fn playing_at(at_ms: u64) -> PlayerState {
        PlayerState {
            track: Some(track(NERVE, "nerve")),
            status: PlayStatus::Playing,
            active_here: true,
            context_uri: Some("spotify:album:census".into()),
            position: Some(Position {
                position_ms: at_ms,
                timestamp_ms: T0_MS,
                speed: 1.0,
            }),
            volume: 40,
            shuffle: false,
            repeat: Repeat::Off,
            actions: Actions {
                play: true,
                pause: true,
                seek: true,
                ..Actions::default()
            },
            liked: Some(false),
            quality: None,
        }
    }

    /// The panel and what it draws from, as the window holds them.
    struct Shown {
        panel: SidePanel,
        view: ViewState,
        bar: BottomBar,
        player: PlayerState,
        ready: bool,
        link: FakeLink,
        art: Option<Art>,
        views: PageViews,
        now: Instant,
        now_ms: i64,
    }

    impl Shown {
        /// Connected, the panel open on `tab`.
        fn new(tab: PanelTab, player: PlayerState) -> Shown {
            let mut panel = SidePanel::default();
            panel.on_connected();
            Shown {
                panel,
                view: ViewState {
                    panel: Some(tab),
                    ..ViewState::default()
                },
                bar: BottomBar::default(),
                player,
                ready: true,
                link: FakeLink::default(),
                art: None,
                views: PageViews::default(),
                now: Instant::now(),
                now_ms: T0_MS,
            }
        }

        /// The engine answers the last request sent, which the panel or the pages
        /// (a queue row's) take, as the window routes it.
        fn answer(&mut self, result: Result<Reply, String>) {
            let id = self.link.last_id();
            if self.panel.wants(id) {
                self.panel.on_reply(id, result);
            } else if self.views.wants(id) {
                self.views.on_reply(id, result);
            } else {
                panic!("nobody's: {id}");
            }
        }
    }

    fn harness(shown: Shown) -> Harness<'static, Shown> {
        let (builder, _) = test_support::builder(vec2(700.0, 600.0), false);
        let mut harness = builder.build_ui_state(
            |ui, s: &mut Shown| {
                let art = s.art.get_or_insert_with(|| Art::offline(ui.ctx().clone()));
                let cx = PanelCx {
                    link: &s.link,
                    view: &mut s.view,
                    art,
                    player: &s.player,
                    ready: s.ready,
                    bar: &mut s.bar,
                    views: &mut s.views,
                    now: s.now,
                    now_ms: s.now_ms,
                };
                show(ui, &mut s.panel, cx);
            },
            shown,
        );
        harness.run();
        harness
    }

    /// The colour `text` was drawn in last frame.
    fn colour(harness: &Harness<'_, Shown>, text: &str) -> Option<Color32> {
        texts(harness)
            .into_iter()
            .find(|(t, _, _)| t == text)
            .map(|(_, c, _)| c)
    }

    /// When the last frame asked to be painted again: `Duration::MAX` for never.
    fn repaint_delay(harness: &Harness<'_, Shown>) -> Duration {
        harness.output().viewport_output[&ViewportId::ROOT].repaint_delay
    }

    /// `clicks` clicks of `button` at `pos`, in one frame: egui counts a double-click by
    /// time.
    fn click_at(harness: &mut Harness<'_, Shown>, pos: Pos2, button: PointerButton, clicks: usize) {
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

    /// A point on the queue row labelled `label`, in its number column.
    fn on_row(harness: &Harness<'_, Shown>, label: &str) -> Pos2 {
        let rect = harness.get_by_label(label).rect();
        pos2(rect.left() + 20.0, rect.center().y)
    }

    #[test]
    fn current_line_cases() {
        let lines = two_lines();
        assert_eq!(current_line(&lines, 0), None);
        assert_eq!(current_line(&lines, 2849), None);
        assert_eq!(current_line(&lines, 2850), Some(0));
        assert_eq!(current_line(&lines, 3000), Some(0));
        assert_eq!(current_line(&lines, 6000), Some(1));
        assert_eq!(current_line(&lines, 600_000), Some(1));
        assert_eq!(current_line(&[], 3000), None);
    }

    /// Carry 3: the highlight moves on time, a repaint at the next line's start, at most a
    /// second away; none after the last line.
    #[test]
    fn line_wait_cases() {
        let lines = two_lines();
        assert_eq!(line_wait(&lines, 0), Some(Duration::from_secs(1)));
        assert_eq!(line_wait(&lines, 2500), Some(Duration::from_millis(350)));
        assert_eq!(line_wait(&lines, 2850), Some(Duration::from_secs(1)));
        assert_eq!(line_wait(&lines, 5990), Some(Duration::from_millis(10)));
        assert_eq!(line_wait(&lines, 6000), None);
        assert_eq!(line_wait(&[], 0), None);
    }

    /// The current line is centred once each time it changes, but not within
    /// `OWNER_HOLD` of the owner scrolling; then it is centred again, even unchanged.
    #[test]
    fn follow_waits_for_the_owner() {
        let t0 = Instant::now();
        let mut follow = Follow::default();
        assert!(follow.centre(NERVE, 0, t0));
        assert!(!follow.centre(NERVE, 0, t0), "once per line");
        assert!(follow.centre(NERVE, 1, t0));
        assert!(follow.centre(OTHER, 1, t0), "another track's");
        follow.owner_scrolled(t0);
        assert!(!follow.centre(OTHER, 2, t0 + OWNER_HOLD - MS));
        assert!(follow.centre(OTHER, 2, t0 + OWNER_HOLD));
        follow.owner_scrolled(t0 + OWNER_HOLD);
        assert!(
            follow.centre(OTHER, 2, t0 + 2 * OWNER_HOLD),
            "scrolled away from"
        );
    }

    /// The current line in the accent, the others secondary; a click seeks to a line's start, and the
    /// highlight moves there at once (the bar holds the position).
    #[test]
    fn click_line_seeks() {
        let mut harness = harness(Shown::new(PanelTab::Lyrics, playing_at(3000)));
        assert_eq!(
            harness.state().link.take(),
            vec![Request::Lyrics {
                track_uri: NERVE.into()
            }]
        );
        harness.get_by_label(LOADING);
        harness
            .state_mut()
            .answer(Ok(Reply::Lyrics(Some(lyrics(true, two_lines())))));
        harness.run();
        assert_eq!(colour(&harness, "first line"), Some(theme::ACCENT));
        assert_eq!(colour(&harness, "second line"), Some(theme::SECONDARY));

        harness.get_by_label("second line").click();
        harness.run();
        let s = harness.state();
        assert_eq!(s.link.take(), vec![Request::Seek { position_ms: 6000 }]);
        assert!(s.bar.wants(s.link.last_id()), "the bar says if it fails");
        assert_eq!(colour(&harness, "second line"), Some(theme::ACCENT));
        assert_eq!(colour(&harness, "first line"), Some(theme::SECONDARY));

        // While the player takes no commands, or can't seek, a line is only words.
        harness.state_mut().ready = false;
        harness.run();
        harness.get_by_label("first line").click();
        harness.run();
        harness.state_mut().ready = true;
        harness.state_mut().player.actions.seek = false;
        harness.run();
        harness.get_by_label("first line").click();
        harness.run();
        assert_eq!(harness.state().link.take(), vec![]);
    }

    /// The current line is kept centred; a turn of the wheel over the lyrics leaves them
    /// where the owner put them for `OWNER_HOLD`, then the current line is centred again.
    #[test]
    fn current_line_centred_unless_owner_scrolled() {
        let lines = (0..80).map(|i| line(i * 1000, &format!("line {i}")));
        let mut harness = harness(Shown::new(PanelTab::Lyrics, playing_at(40_500)));
        harness
            .state_mut()
            .answer(Ok(Reply::Lyrics(Some(lyrics(true, lines.collect())))));
        harness.run();
        // The middle of the lyrics' area: under the tabs, over the panel's margin.
        let top = harness.get_by_label(LYRICS).rect().bottom() + 12.0;
        let middle = (top + 600.0 - 12.0) / 2.0;
        let off_centre = |harness: &Harness<'_, Shown>, label: &str| {
            (harness.get_by_label(label).rect().center().y - middle).abs()
        };
        assert!(off_centre(&harness, "line 40") < 20.0);

        let over = pos2(700.0 - WIDTH / 2.0, middle);
        let events = &mut harness.input_mut().events;
        events.push(Event::PointerMoved(over));
        // Far enough to move line 40 off centre, not so far that line 41 leaves the
        // panel (a JetBrains Mono line is 1.32 of its size).
        events.push(Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta: vec2(0.0, 200.0),
            phase: TouchPhase::Move,
            source: MouseWheelSource::Wheel,
            modifiers: Modifiers::NONE,
        });
        harness.run();
        assert!(
            off_centre(&harness, "line 40") > 150.0,
            "scrolled by the owner"
        );

        // A second on, the next line isn't centred: the owner scrolled just now.
        let advance = |harness: &mut Harness<'_, Shown>, by: Duration| {
            let s = harness.state_mut();
            s.now += by;
            s.now_ms += i64::try_from(by.as_millis()).unwrap();
            harness.run();
        };
        advance(&mut harness, Duration::from_secs(1));
        assert_eq!(colour(&harness, "line 41"), Some(theme::ACCENT));
        assert!(off_centre(&harness, "line 41") > 100.0);
        // Five seconds after the scroll, the current line is centred again.
        advance(&mut harness, Duration::from_secs(4));
        assert!(off_centre(&harness, "line 45") < 20.0);
    }

    /// Carry 3: while playing, a repaint when the next line starts; none while paused.
    #[test]
    fn highlight_repaints_on_the_next_line() {
        let mut harness = harness(Shown::new(PanelTab::Lyrics, playing_at(2500)));
        harness
            .state_mut()
            .answer(Ok(Reply::Lyrics(Some(lyrics(true, two_lines())))));
        harness.run();
        assert_eq!(repaint_delay(&harness), Duration::from_millis(350));
        harness.state_mut().player.status = PlayStatus::Paused;
        harness.run();
        assert_eq!(repaint_delay(&harness), Duration::MAX);
    }

    /// Carry 2: no lyrics (`None`: a 404, or none at all) say so; so do lyrics of nothing
    /// but empty lines, and a track that isn't one (a local file, an episode, nothing),
    /// which isn't asked about.
    #[test]
    fn no_lyrics_text() {
        let mut harness = harness(Shown::new(PanelTab::Lyrics, playing_at(3000)));
        harness.state_mut().answer(Ok(Reply::Lyrics(None)));
        harness.run();
        harness.get_by_label(NO_LYRICS);

        let s = harness.state_mut();
        s.player.track = Some(track(OTHER, "other"));
        harness.run();
        harness
            .state_mut()
            .answer(Ok(Reply::Lyrics(Some(lyrics(true, vec![line(0, " ")])))));
        harness.run();
        harness.get_by_label(NO_LYRICS);

        for uri in [Some("spotify:local:a:b:c:1"), None] {
            let s = harness.state_mut();
            s.player.track = uri.map(|uri| track(uri, "local"));
            harness.run();
            harness.get_by_label(NO_LYRICS);
        }
        let s = harness.state();
        assert_eq!(
            s.link.take(),
            vec![
                Request::Lyrics {
                    track_uri: NERVE.into()
                },
                Request::Lyrics {
                    track_uri: OTHER.into()
                }
            ]
        );
    }

    /// Unsynced lyrics are plain text: every line in the text grey, none current, a click
    /// seeking nowhere.
    #[test]
    fn unsynced_lyrics_are_plain() {
        let mut harness = harness(Shown::new(PanelTab::Lyrics, playing_at(3000)));
        let lines = vec![line(0, "first line"), line(0, "second line")];
        harness
            .state_mut()
            .answer(Ok(Reply::Lyrics(Some(lyrics(false, lines)))));
        harness.run();
        harness.state().link.take();
        assert_eq!(colour(&harness, "first line"), Some(theme::TEXT));
        assert_eq!(colour(&harness, "second line"), Some(theme::TEXT));
        harness.get_by_label("first line").click();
        harness.run();
        assert_eq!(harness.state().link.take(), vec![]);
        assert_eq!(repaint_delay(&harness), Duration::MAX);
    }

    /// Carry 2: asked for once per track and kept for the session; an answer the
    /// connection's loss swallowed is asked for again once connected; a failure stays in
    /// the tab, with Retry.
    #[test]
    fn lyrics_asked_once_per_track() {
        let mut harness = harness(Shown::new(PanelTab::Lyrics, playing_at(3000)));
        let ask = |uri: &str| Request::Lyrics {
            track_uri: uri.into(),
        };
        assert_eq!(harness.state().link.take(), vec![ask(NERVE)]);
        harness.run();
        assert_eq!(harness.state().link.take(), vec![], "once");

        // The connection goes before the answer: nothing is sent meanwhile, and once
        // connected it is asked for again.
        harness.state_mut().panel.on_disconnected();
        harness.state_mut().player.track = Some(track(OTHER, "other"));
        harness.run();
        harness.get_by_label(LOADING);
        harness.state_mut().player.track = Some(track(NERVE, "nerve"));
        harness.run();
        assert_eq!(harness.state().link.take(), vec![]);
        harness.state_mut().panel.on_connected();
        harness.run();
        assert_eq!(harness.state().link.take(), vec![ask(NERVE)]);
        harness.state_mut().answer(Ok(Reply::Lyrics(None)));

        // Another track is asked about; back to the first, it is kept, even across a
        // reconnection.
        harness.state_mut().player.track = Some(track(OTHER, "other"));
        harness.run();
        assert_eq!(harness.state().link.take(), vec![ask(OTHER)]);
        // The link failing for want of a connection isn't the track's failure.
        harness
            .state_mut()
            .answer(Err(IpcError::Closed.to_string()));
        harness.run();
        harness.get_by_label(LOADING);
        harness.state_mut().player.track = Some(track(NERVE, "nerve"));
        harness.state_mut().panel.on_connected();
        harness.run();
        harness.get_by_label(NO_LYRICS);
        assert_eq!(harness.state().link.take(), vec![]);

        // The engine's failure stays in the tab; Retry asks again.
        harness.state_mut().player.track = Some(track(OTHER, "other"));
        harness.run();
        assert_eq!(harness.state().link.take(), vec![ask(OTHER)]);
        let failure = "Couldn't load the lyrics: 500 Internal Server Error";
        harness.state_mut().answer(Err(failure.into()));
        harness.run();
        harness.get_by_label(failure);
        harness.get_by_label(RETRY).click();
        harness.run();
        assert_eq!(harness.state().link.take(), vec![ask(OTHER)]);
        harness.get_by_label(LOADING);
        harness.state_mut().answer(Ok(Reply::Credits(vec![])));
        harness.run();
        harness.get_by_label(OTHER_REPLY);
    }

    /// Role headings with their names below; none (incl. a credits 404) → `No credits`.
    /// The track `Show credits` picked is shown until the tab is left, else the current
    /// track.
    #[test]
    fn credits_roles() {
        let mut harness = harness(Shown::new(PanelTab::Credits, playing_at(3000)));
        let ask = |uri: &str| Request::Credits {
            track_uri: uri.into(),
        };
        assert_eq!(harness.state().link.take(), vec![ask(NERVE)]);
        let roles = vec![
            CreditRole {
                role: "Performers".into(),
                names: vec!["Jane Remover".into(), "2hollis".into()],
            },
            CreditRole {
                role: "Writers".into(),
                names: vec!["Jane Remover".into()],
            },
            CreditRole {
                role: "Producers".into(),
                names: vec![],
            },
        ];
        harness.state_mut().answer(Ok(Reply::Credits(roles)));
        harness.run();
        let performers = harness.get_by_label("Performers").rect();
        let jane = harness
            .query_all_by_label("Jane Remover")
            .next()
            .unwrap()
            .rect();
        let writers = harness.get_by_label("Writers").rect();
        assert!(performers.bottom() <= jane.top() && jane.bottom() <= writers.top());
        assert_eq!(harness.query_all_by_label("Jane Remover").count(), 2);
        harness.get_by_label("2hollis");
        assert!(
            harness.query_by_label("Producers").is_none(),
            "no names, no heading"
        );

        // A track picked with `Show credits`: its credits, asked for once.
        harness.state_mut().views.credits = Some(OTHER.into());
        harness.run();
        assert_eq!(harness.state().link.take(), vec![ask(OTHER)]);
        harness.state_mut().answer(Ok(Reply::Credits(vec![])));
        harness.run();
        harness.get_by_label(NO_CREDITS);
        assert!(harness.query_by_label("Performers").is_none());

        // Leaving the tab drops the pick: back on it, the current track's, kept.
        harness.get_by_label(LYRICS).click();
        harness.run();
        assert_eq!(harness.state().view.panel, Some(PanelTab::Lyrics));
        assert_eq!(harness.state().views.credits, None);
        harness.state().link.take();
        harness.get_by_label(CREDITS).click();
        harness.run();
        harness.get_by_label("Performers");
        assert_eq!(harness.state().link.take(), vec![]);
    }

    /// The tabs, exactly; the panel is drawn only while open.
    #[test]
    fn tabs_and_toggle() {
        let mut harness = harness(Shown::new(PanelTab::Queue, playing_at(3000)));
        for label in [QUEUE, LYRICS, CREDITS] {
            harness.get_by_label(label);
        }
        // The tab shown is white over a thin line in the accent; the others secondary.
        assert_eq!(colour(&harness, QUEUE), Some(theme::TEXT));
        assert_eq!(colour(&harness, LYRICS), Some(theme::SECONDARY));
        assert_eq!(colour(&harness, CREDITS), Some(theme::SECONDARY));
        let queue = harness.get_by_label(QUEUE).rect().expand(1.0);
        assert_eq!(test_support::marks_within(&harness, queue), [theme::ACCENT]);
        let lyrics = harness.get_by_label(LYRICS).rect().expand(1.0);
        assert_eq!(test_support::marks_within(&harness, lyrics), []);
        harness.get_by_label(NOTHING_QUEUED);
        harness.get_by_label(LYRICS).click();
        harness.run();
        assert_eq!(harness.state().view.panel, Some(PanelTab::Lyrics));

        // The toggle closes it, and opens it again on the tab it last showed.
        let s = harness.state_mut();
        s.panel.toggle(&mut s.view.panel);
        assert_eq!(s.view.panel, None);
        harness.run();
        assert!(harness.query_by_label(QUEUE).is_none());
        let s = harness.state_mut();
        s.panel.toggle(&mut s.view.panel);
        assert_eq!(s.view.panel, Some(PanelTab::Lyrics));
        let mut fresh = SidePanel::default();
        let mut closed = None;
        fresh.toggle(&mut closed);
        assert_eq!(closed, Some(PanelTab::Queue), "first opened on the queue");
    }

    /// The queue as track rows with their covers; their hearts asked about once per
    /// change, only those not known; a double-click plays nothing; the row's menu works.
    #[test]
    fn queue_rows() {
        let mut shown = Shown::new(PanelTab::Queue, playing_at(3000));
        let (a, b) = ("spotify:track:a", "spotify:track:b");
        shown.views.liked_set.set(b.into(), true);
        shown
            .panel
            .set_queue(&[track(a, "alpha"), track(b, "beta"), track(a, "alpha")]);
        let mut harness = harness(shown);
        assert_eq!(
            harness.state().link.take(),
            vec![Request::LikedState {
                uris: vec![a.into()]
            }]
        );
        assert_eq!(harness.query_all_by_label("alpha").count(), 2);
        harness.get_by_label("beta");
        harness.run();
        assert_eq!(harness.state().link.take(), vec![], "asked once");

        let pos = on_row(&harness, "beta");
        click_at(&mut harness, pos, PointerButton::Primary, 2);
        assert_eq!(
            harness.state().link.take(),
            vec![],
            "a double-click does nothing"
        );

        click_at(&mut harness, pos, PointerButton::Secondary, 1);
        harness.get_by_label(SHOW_CREDITS).click();
        harness.run();
        let s = harness.state();
        assert_eq!(s.view.panel, Some(PanelTab::Credits));
        assert_eq!(s.views.credits.as_deref(), Some(b));
        let asked = s.link.take();
        assert_eq!(
            asked,
            vec![Request::Credits {
                track_uri: b.into()
            }]
        );

        // A changed queue is asked about again; a reconnection too.
        let s = harness.state_mut();
        s.view.panel = Some(PanelTab::Queue);
        s.panel.set_queue(&[track(OTHER, "other")]);
        s.link.take();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            vec![Request::LikedState {
                uris: vec![OTHER.into()]
            }]
        );
        harness.state_mut().panel.on_connected();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            vec![Request::LikedState {
                uris: vec![OTHER.into()]
            }]
        );

        // Final review C1: once the engine has read Liked Songs every queued heart is
        // asked about again, those the window thought it knew too.
        let s = harness.state_mut();
        let asked = s.link.last_id();
        s.views.on_reply(asked, Ok(Reply::LikedState(vec![None])));
        s.views.liked_set.set(OTHER.into(), true);
        s.views.liked_set.on_liked_set_ready();
        s.panel.on_liked_set_ready();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            vec![Request::LikedState {
                uris: vec![OTHER.into()]
            }]
        );
    }

    /// A queue row's heart, add to queue and radio go to the engine; a failure is logged,
    /// its answer taken.
    #[test]
    fn queue_row_requests() {
        let mut shown = Shown::new(PanelTab::Queue, playing_at(3000));
        shown.panel.set_queue(&[track(OTHER, "other")]);
        let mut harness = harness(shown);
        harness.state().link.take();
        harness.get_by_label(HEART_LABEL).click();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            vec![Request::SetLiked {
                uri: OTHER.into(),
                liked: true
            }]
        );
        harness.state_mut().answer(Ok(Reply::Liked {
            uri: OTHER.into(),
            liked: true,
        }));
        assert_eq!(harness.state().views.liked_set.get(OTHER), Some(true));
    }
}
