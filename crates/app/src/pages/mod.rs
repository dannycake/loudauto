//! The pages of the main area: Home, Search, an artist's, an album's, a playlist's,
//! Liked Songs and Recently played. Each is drawn inside the page frame (loading, stale,
//! failed and Retry, from `page_frame`) from the engine's reply, and only from memory.
//!
//! Cards open their page (a track's plays); a card's play button plays it. Track lists
//! are `track_list::TrackList`s, their hearts filled from one `LikedState` request per
//! page reply. A card or a link the pointer rests on for [`HOVER`] has its page
//! prefetched, once.

mod album;
mod artist;
mod cards;
mod home;
mod liked;
mod playlist;
mod recents;
pub mod search;

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use eframe::egui::{
    CursorIcon, Rect, RichText, Role, ScrollArea, Sense, TextStyle, TextWrapMode, Ui, Vec2,
    WidgetInfo, WidgetText, pos2, vec2,
};
use engine_ipc::{PageReply, PageRequest, PlayerState, Reply, Request};
use log::{debug, warn};
use playback::{
    command::Repeat,
    event::{Actions, PlayStatus},
};
use spot_core::pages::{Image, TrackRow};

pub use cards::{LIKED_SONGS_URI, card_page, card_play};
pub use search::{DEBOUNCE, Debounce, SearchBox};

use crate::{
    art::Art,
    link::{RequestId, Requests},
    nav::Nav,
    notices::{self, failure_text},
    page_frame::{self, LOADING, Pages},
    theme::{self, ROW_HEIGHT},
    track_list::{LinkTarget, RowAction, Selection, TrackList, paint_link, play_request},
    view_state::{PanelTab, ViewState, scroll_key},
};

/// How long the pointer rests on a card or a link before its page is prefetched.
pub const HOVER: Duration = Duration::from_millis(150);
/// What a reply of another kind than the page asked for says.
pub const OTHER_PAGE: &str = "The engine sent a different page from the one asked for.";
/// What a track list with no tracks says.
pub const NO_SONGS: &str = "No songs";

/// What the pages keep between frames, beside the pages themselves.
#[derive(Debug, Default)]
pub struct PageViews {
    pub liked_set: LikedSet,
    /// The current page's track list's selection; a page landed on starts with none.
    pub selection: Selection,
    pub hover: HoverPrefetch,
    pub search: SearchBox,
    /// The track `Show credits` picked last, for the right panel's Credits tab.
    pub credits: Option<String>,
    /// What each request the pages sent was, to say so if it fails.
    sent: HashMap<RequestId, &'static str>,
    /// The page drawn last frame, to tell a landing on a page from a frame.
    shown: Option<PageRequest>,
}

impl PageViews {
    /// Whether the reply to request `id` is for the pages.
    pub fn wants(&self, id: RequestId) -> bool {
        self.liked_set.wants(id) || self.sent.contains_key(&id)
    }

    /// The reply to request `id`, which [`PageViews::wants`]: when a play, a heart, a
    /// queue or a radio failed, what to tell the owner (`Couldn't play: …`).
    pub fn on_reply(&mut self, id: RequestId, result: Result<Reply, String>) -> Option<String> {
        if self.liked_set.wants(id) {
            self.liked_set.on_reply(id, result);
            return None;
        }
        let what = self.sent.remove(&id)?;
        match result {
            Ok(Reply::Liked { uri, liked }) => self.liked_set.set(uri, liked),
            Ok(_) => {}
            Err(e) => {
                warn!("couldn't {what}: {e}");
                return Some(failure_text(what, &e));
            }
        }
        None
    }

    /// The link (re)connected: each page's hearts are asked about afresh.
    pub fn on_connected(&mut self) {
        self.liked_set.on_connected();
        self.sent.clear();
    }

    /// Does what the owner did to a row of a list beside the pages (the right panel's
    /// queue) as a page's row would, playing aside: whether a row plays is its list's
    /// call. A request's failure is said as a page's.
    pub(crate) fn row_action(
        &mut self,
        action: RowAction,
        link: &dyn Requests,
        nav: &mut Nav,
        panel: &mut Option<PanelTab>,
    ) {
        act_on_row(action, link, &mut self.sent, nav, panel, &mut self.credits);
    }
}

/// What the window lends the pages for a frame.
pub struct Env<'a> {
    pub link: &'a dyn Requests,
    pub view: &'a mut ViewState,
    pub art: &'a mut Art,
    pub player: &'a PlayerState,
    /// The account's name, once the engine has said: Liked Songs play in its context.
    pub username: Option<&'a str>,
    pub now: Instant,
}

/// What a page draws with, and what it may change.
pub struct PageCx<'a> {
    pub link: &'a dyn Requests,
    pub nav: &'a mut Nav,
    /// The page being drawn: its request.
    pub page: PageRequest,
    /// Each page's scroll offset, by [`scroll_key`] (the saved view's).
    pub scroll: &'a mut HashMap<String, f32>,
    /// The right panel's tab (the saved view's).
    pub panel: &'a mut Option<PanelTab>,
    pub art: &'a mut Art,
    pub player: &'a PlayerState,
    pub username: Option<&'a str>,
    pub selection: &'a mut Selection,
    pub liked_set: &'a mut LikedSet,
    /// The track whose credits the Credits tab shows.
    pub credits: &'a mut Option<String>,
    /// The page the card or link under the pointer opens, written as the page draws:
    /// what the hover prefetch watches.
    pub hovered: Option<PageRequest>,
    pub now: Instant,
    sent: &'a mut HashMap<RequestId, &'static str>,
}

impl PageCx<'_> {
    /// Sends `req`; `what` says what it was ("play", …) should it fail.
    pub fn send(&mut self, what: &'static str, req: Request) {
        let id = self.link.send(req);
        self.sent.insert(id, what);
    }
}

/// The player before the engine has said anything: nothing playing.
pub fn idle_player() -> PlayerState {
    PlayerState {
        track: None,
        status: PlayStatus::Paused,
        active_here: false,
        context_uri: None,
        position: None,
        volume: 0,
        shuffle: false,
        repeat: Repeat::Off,
        actions: Actions::default(),
        liked: None,
        quality: None,
    }
}

/// Draws the current page into the rest of `ui`: asks for it on landing, then draws it
/// in its frame. The page a card or link has been hovered for [`HOVER`] is prefetched.
pub fn show(ui: &mut Ui, pages: &mut Pages, views: &mut PageViews, env: Env<'_>) {
    let Env {
        link,
        view,
        art,
        player,
        username,
        now,
    } = env;
    let current = view.nav.current().clone();
    pages.showing(link, &current);
    let previous = views.shown.replace(current.clone());
    if previous.as_ref() != Some(&current) {
        views.selection = Selection::default();
    }
    let PageViews {
        liked_set,
        selection,
        hover,
        search,
        credits,
        sent,
        ..
    } = views;
    let ViewState {
        nav, scroll, panel, ..
    } = view;
    let mut cx = PageCx {
        link,
        nav,
        page: current.clone(),
        scroll,
        panel,
        art,
        player,
        username,
        selection,
        liked_set,
        credits,
        hovered: None,
        now,
        sent,
    };
    match &current {
        PageRequest::Search { query } => {
            search::show(ui, query, previous.as_ref(), pages, search, &mut cx);
        }
        // Never a page of the main area.
        PageRequest::Sidebar => {}
        req => framed(ui, req, pages, &mut cx),
    }
    // A card or a link opened a page: it is drawn on the next frame.
    if *cx.nav.current() != current {
        ui.ctx().request_repaint();
    }
    let hovered = cx.hovered.take().filter(prefetches_on_hover);
    if let Some(req) = hover.hover(hovered, now) {
        pages.prefetch(link, &req);
    }
    if let Some(wait) = hover.wait(now) {
        ui.ctx().request_repaint_after(wait);
    }
}

/// `req`'s page in its frame: Loading, Failed with Retry, or the page under its stale
/// line. `cx.page` is `req`.
fn framed(ui: &mut Ui, req: &PageRequest, pages: &mut Pages, cx: &mut PageCx) {
    let mut retry = false;
    match pages.get(req) {
        Some(state) => {
            page_frame::show(ui, state, || retry = true, |ui, reply| body(ui, reply, cx))
        }
        // Not asked for yet (a search still being typed).
        None => {
            ui.label(RichText::new(LOADING).color(theme::SECONDARY));
        }
    }
    if retry {
        pages.retry(cx.link, req);
    }
}

/// The page `reply` is, drawn as the page `cx.page` asked for.
fn body(ui: &mut Ui, reply: &PageReply, cx: &mut PageCx) {
    match &cx.page {
        PageRequest::Home => home::show(ui, reply, cx),
        PageRequest::Search { .. } => search::results(ui, reply, cx),
        PageRequest::Artist { .. } => artist::show(ui, reply, cx),
        PageRequest::Album { .. } => album::show(ui, reply, cx),
        PageRequest::Playlist { .. } => playlist::show(ui, reply, cx),
        PageRequest::Liked => liked::show(ui, reply, cx),
        PageRequest::Recents => recents::show(ui, reply, cx),
        PageRequest::Sidebar => {}
    }
}

/// A reply that isn't the page asked for: the engine's mistake, in words, in the error's
/// red box.
fn other_page(ui: &mut Ui) {
    notices::error_box(ui, |ui| {
        ui.horizontal(|ui| {
            page_frame::warning_icon(ui);
            ui.label(OTHER_PAGE);
        });
    });
}

/// Which tracks are liked, as far as the engine has said: one `LikedState` request per
/// page reply, the answer to a heart, and `LikedChanged`. Unknown is drawn as not liked,
/// except on Liked Songs, whose rows are liked by definition; its copies never write
/// here, as a cached one can still hold a track unliked since.
///
/// An engine that hasn't read Liked Songs yet answers unknown for what it hasn't
/// checked: that leaves the window's last word (from before an engine restart, say) in
/// place, and once the engine has read it (`Event::LikedSetReady`) everything is asked
/// about again. A heart never toggles: it asks for the state it didn't show.
#[derive(Debug, Default)]
pub struct LikedSet {
    liked: HashMap<String, bool>,
    /// Those of `liked` the engine has answered since the link connected and since it
    /// read Liked Songs; the rest are carried over.
    current: HashSet<String>,
    /// `LikedState` requests out, and the tracks each asked about, in order.
    pending: HashMap<RequestId, Vec<String>>,
    /// The copy (its `fetched_at_ms`) of each page already asked about.
    asked: HashMap<PageRequest, i64>,
}

impl LikedSet {
    pub fn get(&self, uri: &str) -> Option<bool> {
        self.liked.get(uri).copied()
    }

    /// Whether the engine has said, as it is now, whether `uri` is liked.
    pub fn is_current(&self, uri: &str) -> bool {
        self.current.contains(uri)
    }

    pub fn set(&mut self, uri: String, liked: bool) {
        self.current.insert(uri.clone());
        self.liked.insert(uri, liked);
    }

    /// Once per copy `reply` of `page` (told apart by `fetched_at_ms`): asks the engine
    /// which of `rows` are liked, in one request. Liked Songs isn't asked about.
    pub fn ask(
        &mut self,
        link: &dyn Requests,
        page: &PageRequest,
        reply: &PageReply,
        rows: &[TrackRow],
    ) {
        if *page == PageRequest::Liked || self.asked.get(page) == Some(&reply.fetched_at_ms) {
            return;
        }
        self.asked.insert(page.clone(), reply.fetched_at_ms);
        let mut seen = HashSet::new();
        let uris: Vec<String> = rows
            .iter()
            .filter(|row| seen.insert(row.uri.as_str()))
            .map(|row| row.uri.clone())
            .collect();
        self.ask_uris(link, uris);
    }

    /// Asks the engine which of `uris` are liked, in one request (none for none): the
    /// right panel's queue asks this way.
    pub fn ask_uris(&mut self, link: &dyn Requests, uris: Vec<String>) {
        if !uris.is_empty() {
            let id = link.send(Request::LikedState { uris: uris.clone() });
            self.pending.insert(id, uris);
        }
    }

    /// Whether the reply to request `id` is a `LikedState` answer for this set.
    pub fn wants(&self, id: RequestId) -> bool {
        self.pending.contains_key(&id)
    }

    pub fn on_reply(&mut self, id: RequestId, result: Result<Reply, String>) {
        let Some(uris) = self.pending.remove(&id) else {
            return;
        };
        match result {
            Ok(Reply::LikedState(answers)) if answers.len() == uris.len() => {
                // Unknown leaves what the window knew.
                for (uri, answer) in uris.into_iter().zip(answers) {
                    if let Some(liked) = answer {
                        self.set(uri, liked);
                    }
                }
            }
            Ok(other) => warn!("the engine answered LikedState with {other:?}"),
            // The hearts stay as they are; the page is asked about again once connected.
            Err(e) => debug!("can't tell which tracks are liked: {e}"),
        }
    }

    /// The link (re)connected: answers still out are lost, and each page is asked about
    /// again when next drawn.
    pub fn on_connected(&mut self) {
        self.pending.clear();
        self.asked.clear();
        self.current.clear();
    }

    /// `Event::LikedSetReady`: the engine knows every heart now, so each page is asked
    /// about again when next drawn.
    pub fn on_liked_set_ready(&mut self) {
        self.asked.clear();
        self.current.clear();
    }
}

/// The hover prefetch: the page the pointer has rested on for [`HOVER`] is asked for
/// ahead of its opening, once per page.
#[derive(Debug, Default)]
pub struct HoverPrefetch {
    /// The page hovered, since when.
    hovered: Option<(PageRequest, Instant)>,
    /// Those prefetched already.
    sent: HashSet<PageRequest>,
}

impl HoverPrefetch {
    /// This frame's hovered page (`None` for nothing); the page to prefetch, once its
    /// hover has lasted [`HOVER`] and the first time only.
    pub fn hover(&mut self, target: Option<PageRequest>, now: Instant) -> Option<PageRequest> {
        let Some(target) = target else {
            self.hovered = None;
            return None;
        };
        let since = match &self.hovered {
            Some((hovered, since)) if *hovered == target => *since,
            _ => {
                self.hovered = Some((target.clone(), now));
                now
            }
        };
        let due = now.saturating_duration_since(since) >= HOVER;
        (due && self.sent.insert(target.clone())).then_some(target)
    }

    /// How long until the hovered page is due, for a repaint then: the pointer resting
    /// still makes no frames of its own.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        let (hovered, since) = self.hovered.as_ref()?;
        if self.sent.contains(hovered) {
            return None;
        }
        let left = HOVER.saturating_sub(now.saturating_duration_since(*since));
        // Due now: the next frame takes it.
        Some(left.max(Duration::from_millis(1)))
    }
}

/// Whether hovering a card or a link for `req` prefetches it: an album's, an artist's or
/// a playlist's page. Never Liked Songs: its page is the whole library, many requests.
pub fn prefetches_on_hover(req: &PageRequest) -> bool {
    match req {
        PageRequest::Album { .. } | PageRequest::Artist { .. } => true,
        PageRequest::Playlist { uri } => uri.starts_with("spotify:playlist:"),
        _ => false,
    }
}

/// A page's track list: the rows, the context they play in, whether they show covers.
struct List<'r> {
    rows: &'r [TrackRow],
    context: Option<&'r str>,
    thumbs: bool,
}

/// Draws `list`, a part of `reply`, and does what the owner did to a row. A page's
/// only list fills the rest of `ui` and scrolls by itself, from the page's saved offset;
/// an `embedded` one (among other sections, scrolled with the page) takes the height of
/// its rows. Asks once per reply which rows are liked.
fn track_list(ui: &mut Ui, reply: &PageReply, list: List<'_>, embedded: bool, cx: &mut PageCx) {
    let List {
        rows,
        context,
        thumbs,
    } = list;
    // Carry 2: a list is shown only with its rows, or a saved offset would be clamped to
    // nothing and written back.
    if rows.is_empty() {
        ui.label(RichText::new(NO_SONGS).color(theme::SECONDARY));
        return;
    }
    cx.liked_set.ask(cx.link, &cx.page, reply, rows);
    let key = scroll_key(&cx.page);
    let player = match (&cx.page, context) {
        (PageRequest::Liked, Some(context)) => liked_player(cx.player, context),
        _ => Cow::Borrowed(cx.player),
    };
    let liked_set = &*cx.liked_set;
    // Liked Songs' rows are liked unless the engine has said otherwise since.
    let by_definition = (cx.page == PageRequest::Liked).then_some(true);
    let liked = |uri: &str| liked_set.get(uri).or(by_definition);
    let (id_salt, scroll) = if embedded {
        (format!("{key}/tracks"), None)
    } else {
        let offset = cx.scroll.entry(key.clone()).or_insert(0.0);
        (key, Some(offset))
    };
    let list = TrackList {
        id_salt: &id_salt,
        rows,
        context_uri: context,
        show_thumbs: thumbs,
        liked: &liked,
        scroll,
    };
    let (selection, art) = (&mut *cx.selection, &mut *cx.art);
    let action = if embedded {
        let size = vec2(ui.available_width(), rows.len() as f32 * ROW_HEIGHT);
        ui.allocate_ui(size, |ui| list.show(ui, &player, selection, art))
            .inner
    } else {
        list.show(ui, &player, selection, art)
    };
    drop(player);
    if let Some(target) = &cx.selection.hovered_link {
        cx.hovered = Some(link_page(target));
    }
    if let Some(action) = action {
        row_action(action, rows, context, cx);
    }
}

/// Does what the owner did to a row of `rows`, which play in `context`.
fn row_action(action: RowAction, rows: &[TrackRow], context: Option<&str>, cx: &mut PageCx) {
    match action {
        RowAction::Play { index } => {
            if let Some(req) = play_request(rows, context, index) {
                cx.send("play", req);
            }
        }
        other => act_on_row(other, cx.link, cx.sent, cx.nav, cx.panel, cx.credits),
    }
}

/// Does what the owner did to a row, playing aside (the row's list decides that): sends
/// the request, kept in `sent` to say so should it fail; opens the page; or picks the
/// track for the right panel's Credits tab.
fn act_on_row(
    action: RowAction,
    link: &dyn Requests,
    sent: &mut HashMap<RequestId, &'static str>,
    nav: &mut Nav,
    panel: &mut Option<PanelTab>,
    credits: &mut Option<String>,
) {
    let mut send = |what, req| {
        sent.insert(link.send(req), what);
    };
    match action {
        RowAction::Play { .. } => {}
        RowAction::Heart { uri, liked } => {
            send("change the heart", Request::SetLiked { uri, liked });
        }
        RowAction::AddToQueue(uri) => send("add to the queue", Request::AddToQueue { uri }),
        RowAction::StartRadio(seed_uri) => {
            send("start the radio", Request::StartRadio { seed_uri });
        }
        // `Nav::go` adds nothing for the page already shown: a link double-clicked says
        // Go twice.
        RowAction::GoArtist(uri) => nav.go(PageRequest::Artist { uri }),
        RowAction::GoAlbum(uri) => nav.go(PageRequest::Album { uri }),
        RowAction::ShowCredits(uri) => {
            *credits = Some(uri);
            *panel = Some(PanelTab::Credits);
        }
        // The list copied it.
        RowAction::CopyLink(_) => {}
    }
}

/// The page a row's link opens.
fn link_page(target: &LinkTarget) -> PageRequest {
    match target {
        LinkTarget::Artist(uri) => PageRequest::Artist { uri: uri.clone() },
        LinkTarget::Album(uri) => PageRequest::Album { uri: uri.clone() },
    }
}

/// `player` as Liked Songs' list sees it: Spotify also names that context
/// `spotify:collection:tracks`, which plays the same rows as `context`.
fn liked_player<'p>(player: &'p PlayerState, context: &str) -> Cow<'p, PlayerState> {
    if player.context_uri.as_deref() == Some(LIKED_SONGS_URI) {
        let mut player = player.clone();
        player.context_uri = Some(context.to_owned());
        Cow::Owned(player)
    } else {
        Cow::Borrowed(player)
    }
}

/// `text` as a link to `to`: white and underlined while hovered. A click opens it; the
/// hover marks it for the prefetch.
fn link(ui: &mut Ui, text: &str, to: PageRequest, cx: &mut PageCx) {
    let galley = WidgetText::from(text).into_galley(
        ui,
        Some(TextWrapMode::Extend),
        f32::INFINITY,
        TextStyle::Body,
    );
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Link, ui.is_enabled(), text));
    let hovered = response.hovered();
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let clip = painter.clip_rect();
        paint_link(painter, clip, rect.min, galley, (theme::TEXT, hovered));
    }
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        cx.hovered = Some(to.clone());
    }
    if response.clicked() {
        cx.nav.go(to);
    }
}

/// A page's sections, scrolled together from where the page was last left; the offset
/// is kept in the saved view.
fn scrolled(ui: &mut Ui, cx: &mut PageCx, add: impl FnOnce(&mut Ui, &mut PageCx)) {
    let key = scroll_key(&cx.page);
    let restored = ui.make_persistent_id(("page-scroll-restored", &key));
    let mut area = ScrollArea::vertical()
        .id_salt(("page", &key))
        .auto_shrink(false);
    // Shown from the saved offset the first time this window shows the page.
    if ui.data(|d| d.get_temp::<()>(restored)).is_none() {
        area = area.vertical_scroll_offset(cx.scroll.get(&key).copied().unwrap_or(0.0));
        ui.data_mut(|d| d.insert_temp(restored, ()));
    }
    let output = area.show(ui, |ui| add(ui, cx));
    cx.scroll.insert(key, output.state.clamped_offset().y);
}

/// A header's cover, in points.
const HEADER_COVER: f32 = 192.0;

/// A page's header: its cover, and beside it what `beside` draws.
fn header(
    ui: &mut Ui,
    images: &[Image],
    cx: &mut PageCx,
    beside: impl FnOnce(&mut Ui, &mut PageCx),
) {
    ui.horizontal(|ui| {
        cover(ui, images, HEADER_COVER, cx);
        ui.add_space(24.0);
        ui.vertical(|ui| beside(ui, cx));
    });
}

/// A square cover `side` points wide: the line grey until it has arrived.
fn cover(ui: &mut Ui, images: &[Image], side: f32, cx: &mut PageCx) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    match cx.art.get(images, side, ui.ctx()) {
        Some(texture) => {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            ui.painter().image(texture.id(), rect, uv, theme::UNTINTED);
        }
        None => {
            ui.painter().rect_filled(rect, 0.0, theme::LINES);
        }
    }
}

/// A page's title.
fn title(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .text_style(TextStyle::Heading)
            .color(theme::TEXT),
    );
}

/// `n songs`, or `1 song`.
fn songs(n: u32) -> String {
    match n {
        1 => "1 song".to_owned(),
        n => format!("{n} songs"),
    }
}

/// `html` as plain text, as playlist and album descriptions and card subtitles come with
/// tags (`<a href=…>`) and entities (`&#x2F;`, `&amp;`): tags dropped, entities decoded,
/// runs of white space made one space.
pub fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find(['<', '&']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if rest.starts_with('<') {
            match tag_len(rest) {
                Some(len) => {
                    // A tag parts words like a space would (`<br>`, `</p>`).
                    out.push(' ');
                    rest = &rest[len..];
                }
                None => {
                    out.push('<');
                    rest = &rest[1..];
                }
            }
        } else {
            match entity(rest) {
                Some((c, len)) => {
                    out.push(c);
                    rest = &rest[len..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The length of the tag `s` starts with (`<` a letter, `/` or `!`, through `>`).
fn tag_len(s: &str) -> Option<usize> {
    let next = s[1..].chars().next()?;
    if !(next.is_ascii_alphabetic() || next == '/' || next == '!') {
        return None;
    }
    Some(s.find('>')? + 1)
}

/// The character the entity `s` starts with stands for, and the entity's length.
fn entity(s: &str) -> Option<(char, usize)> {
    // Entities are short and ASCII: the `;` within the first dozen bytes.
    let end = s.bytes().take(12).position(|b| b == b';')?;
    let name = &s[1..end];
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)?
        }
    };
    Some((c, end + 1))
}

#[cfg(test)]
mod tests;
