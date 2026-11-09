//! A page's states and the frame drawn around its body, so a page is never blank:
//! loading (a secondary `Loading…`), loaded, stale (the engine's cached copy, under a
//! secondary line with its age) and failed (`⚠`, the message and a Retry button in the
//! error's red box).
//!
//! [`Pages`] holds every page the engine has sent, by request: those the window asked
//! for, and those it announced unasked (`Event::PageUpdated` for a page hovered or
//! fetched on after its caller gave up), so opening one later is instant.
//!
//! The link doesn't queue requests while there is no connection, so `Pages` sends none
//! then, and a request the connection's loss failed isn't a failed page: after each
//! `Connected` the current page and the sidebar are asked for again, and the rest when
//! they are next opened. What failed for want of the session, or of the network, is
//! asked for again once that is back (`refresh_failing`, `take_recovered`).

use std::{
    cmp::Ordering,
    collections::HashMap,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use eframe::egui::{Response, RichText, Role, Sense, Shape, Stroke, Ui, Vec2, WidgetInfo, pos2};
use engine_ipc::{IpcError, PageReply, PageRequest, Reply, Request};
use log::{debug, warn};

use crate::{
    link::{NOT_CONNECTED, RequestId, Requests},
    notices, theme,
};

/// What a loading page says.
pub const LOADING: &str = "Loading…";
/// The icon beside an error.
pub const WARNING: &str = "⚠";
/// The button that asks for a failed page again.
pub const RETRY: &str = "Retry";

/// A page, as far as the window knows it.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "Ready, the large variant, is what most pages are: boxing it would allocate per \
              page to shrink the few loading or failed"
)]
pub enum PageState {
    /// Asked for (or to be, once connected) since then.
    Loading { since: Instant },
    /// The page; stale when it is the engine's cached copy.
    Ready(PageReply),
    /// Why there is no page.
    Failed(String),
}

/// A page request out with the engine.
#[derive(Debug)]
struct Pending {
    req: PageRequest,
    prefetch: bool,
}

/// The pages, by request, and the requests out for them.
#[derive(Debug, Default)]
pub struct Pages {
    states: HashMap<PageRequest, PageState>,
    pending: HashMap<RequestId, Pending>,
    connected: bool,
    /// The page the window showed last, to tell a landing on a page from a frame.
    shown: Option<PageRequest>,
    /// A page failed, or came with why its refresh failed, since the last fresh copy.
    troubled: bool,
    /// A fresh copy came while `troubled`: Spotify answers again (`take_recovered`).
    recovered: bool,
}

impl Pages {
    pub fn new() -> Pages {
        Pages::default()
    }

    /// Whether `req` is a page the engine is asked for: not a search with no words yet.
    pub fn fetches(req: &PageRequest) -> bool {
        !matches!(req, PageRequest::Search { query } if query.trim().is_empty())
    }

    pub fn get(&self, req: &PageRequest) -> Option<&PageState> {
        self.states.get(req)
    }

    /// Asks for `req` unless the window has it or has asked for it already. Until the
    /// link is connected the page is Loading and asked for once it is.
    pub fn ensure(&mut self, link: &(impl Requests + ?Sized), req: &PageRequest) {
        if Self::fetches(req) && !self.states.contains_key(req) {
            self.load(link, req);
        }
    }

    /// The window shows `req` this frame. Once per landing on a page (opened, or come
    /// back to), it is asked for afresh even if loaded: the engine refreshes its cached
    /// copy only when asked, answering with it (stale) at once and announcing the new
    /// one. The copy on screen stays meanwhile.
    pub fn showing(&mut self, link: &(impl Requests + ?Sized), req: &PageRequest) {
        if self.shown.as_ref() == Some(req) {
            return self.ensure(link, req);
        }
        self.shown = Some(req.clone());
        if !Self::fetches(req) {
            return;
        }
        match self.states.get(req) {
            None | Some(PageState::Failed(_)) => self.load(link, req),
            // Asked for already, or to be once connected.
            Some(PageState::Loading { .. }) => {}
            Some(PageState::Ready(_)) => {
                let asking = self.pending.values().any(|p| &p.req == req && !p.prefetch);
                if self.connected && !asking {
                    self.send(link, req, false);
                }
            }
        }
    }

    /// Asks for `req` again (its Retry button).
    pub fn retry(&mut self, link: &(impl Requests + ?Sized), req: &PageRequest) {
        if Self::fetches(req) {
            self.load(link, req);
        }
    }

    /// Asks again for those of `reqs` that failed, or whose copy on screen says why its
    /// refresh failed, unless they are being asked for already: what was asked for
    /// before the session was ready, or while the network was down.
    pub fn refresh_failing(&mut self, link: &(impl Requests + ?Sized), reqs: &[PageRequest]) {
        for req in reqs {
            let asking = self.pending.values().any(|p| &p.req == req && !p.prefetch);
            match self.states.get(req) {
                Some(PageState::Failed(_)) => self.load(link, req),
                Some(PageState::Ready(reply))
                    if reply.error.is_some() && !asking && self.connected =>
                {
                    self.send(link, req, false);
                }
                _ => {}
            }
        }
    }

    /// Whether a fresh copy of a page has come since a page last failed (or came with an
    /// error): Spotify answers again, so what failed is worth asking for again. Once.
    pub fn take_recovered(&mut self) -> bool {
        std::mem::take(&mut self.recovered)
    }

    /// Asks for `req` ahead of its opening, if the window hasn't it nor asked for it:
    /// the engine may refuse (`Reply::Dropped`), which leaves the page as it was.
    pub fn prefetch(&mut self, link: &(impl Requests + ?Sized), req: &PageRequest) {
        let asked = self.pending.values().any(|p| &p.req == req);
        if self.connected && Self::fetches(req) && !self.states.contains_key(req) && !asked {
            self.send(link, req, true);
        }
    }

    /// Whether the reply to request `id` is for `Pages`.
    pub fn wants(&self, id: RequestId) -> bool {
        self.pending.contains_key(&id)
    }

    /// The reply to request `id`, which [`Pages::wants`].
    pub fn on_reply(&mut self, id: RequestId, result: Result<Reply, String>) {
        let Some(Pending { req, prefetch }) = self.pending.remove(&id) else {
            return;
        };
        let error = match result {
            Ok(Reply::Page(reply)) => return self.store(req, reply),
            // A refused prefetch: nothing changes.
            Ok(Reply::Dropped) => return,
            Ok(other) => {
                warn!("the engine answered a page request with {other:?}");
                "The engine sent something other than a page.".to_string()
            }
            Err(e) => e,
        };
        if prefetch {
            debug!("prefetch of {req:?} failed: {error}");
        } else if is_connection_error(&error) {
            // Not the page's failure: it stays Loading, and is asked again once the
            // link is back.
            debug!("{req:?}: {error}; asking again once connected");
        } else if let Some(PageState::Ready(_)) = self.states.get(&req) {
            // The copy on screen stays; the engine says why when it announces it again.
            debug!("asking for {req:?} again failed: {error}");
            self.troubled = true;
        } else {
            self.states.insert(req, PageState::Failed(error));
            self.troubled = true;
        }
    }

    /// `Event::PageUpdated`: a fresh copy, or the cached one again with why its refresh
    /// failed; for any page, open or not.
    pub fn on_event(&mut self, req: PageRequest, reply: PageReply) {
        self.store(req, reply);
    }

    /// The link (re)connected: what failed or waited before is asked afresh when next
    /// opened, and `current` and the sidebar are asked for now, loaded or not (the
    /// engine may have restarted, and its copies be newer).
    pub fn on_connected(&mut self, link: &(impl Requests + ?Sized), current: &PageRequest) {
        self.connected = true;
        self.states
            .retain(|_, state| matches!(state, PageState::Ready(_)));
        for req in [current, &PageRequest::Sidebar] {
            if !Self::fetches(req) {
                continue;
            }
            if self.states.contains_key(req) {
                self.send(link, req, false);
            } else {
                self.load(link, req);
            }
        }
    }

    pub fn on_disconnected(&mut self) {
        self.connected = false;
    }

    /// Marks `req` Loading and asks for it, if connected.
    fn load(&mut self, link: &(impl Requests + ?Sized), req: &PageRequest) {
        self.states.insert(
            req.clone(),
            PageState::Loading {
                since: Instant::now(),
            },
        );
        if self.connected {
            self.send(link, req, false);
        }
    }

    fn send(&mut self, link: &(impl Requests + ?Sized), req: &PageRequest, prefetch: bool) {
        let id = link.send(Request::Page {
            req: req.clone(),
            prefetch,
        });
        self.pending.insert(
            id,
            Pending {
                req: req.clone(),
                prefetch,
            },
        );
    }

    /// Keeps `reply` as `req`'s page unless the window has a newer copy.
    fn store(&mut self, req: PageRequest, reply: PageReply) {
        if reply.error.is_some() {
            self.troubled = true;
        } else if !reply.stale && std::mem::take(&mut self.troubled) {
            self.recovered = true;
        }
        if let Some(PageState::Ready(old)) = self.states.get(&req)
            && !replaces(&reply, old)
        {
            return;
        }
        self.states.insert(req, PageState::Ready(reply));
    }
}

/// Whether `new` should replace `old`, both copies of one page: a later copy does, an
/// earlier one never; the same copy again only to say it is stale, or why its refresh
/// failed.
fn replaces(new: &PageReply, old: &PageReply) -> bool {
    match new.fetched_at_ms.cmp(&old.fetched_at_ms) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => new.error.is_some() || !new.stale || old.stale,
    }
}

/// Whether `error` is the link's, for want of a connection, rather than the engine's.
pub fn is_connection_error(error: &str) -> bool {
    error == NOT_CONNECTED || error == IpcError::Closed.to_string()
}

/// Draws `state`: `body` for a page, under the stale line when it is a cached copy; a
/// failure as `⚠`, its message and a Retry button (which calls `retry`) in the error's red
/// box.
pub fn show(
    ui: &mut Ui,
    state: &PageState,
    retry: impl FnOnce(),
    body: impl FnOnce(&mut Ui, &PageReply),
) {
    match state {
        PageState::Loading { .. } => {
            ui.label(RichText::new(LOADING).color(theme::SECONDARY));
        }
        PageState::Failed(message) => {
            let clicked = notices::error_box(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    warning_icon(ui);
                    ui.label(message.as_str());
                });
                theme::main_button(ui, RETRY).clicked()
            });
            if clicked {
                retry();
            }
        }
        PageState::Ready(reply) => {
            if reply.stale {
                let line = stale_line(reply, now_ms());
                ui.label(RichText::new(line).color(theme::SECONDARY));
            }
            body(ui, reply);
        }
    }
}

/// `⚠`, drawn rather than set: egui's bundled fonts have no U+26A0, and the symbol font
/// may lack it too. A triangle outline with an exclamation mark, [`theme::ICON`] square,
/// in the text's colour (the error's light red in its box).
pub fn warning_icon(ui: &mut Ui) -> Response {
    let side = theme::ICON;
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Image, ui.is_enabled(), WARNING));
    if ui.is_rect_visible(rect) {
        let colour = ui.visuals().text_color();
        let r = rect.shrink(1.0);
        let stroke = Stroke::new(1.5, colour);
        let at = |y: f32| pos2(r.center().x, r.top() + r.height() * y);
        let painter = ui.painter();
        painter.add(Shape::closed_line(
            vec![at(0.0), r.right_bottom(), r.left_bottom()],
            stroke,
        ));
        painter.line_segment([at(0.38), at(0.68)], stroke);
        painter.circle_filled(at(0.84), stroke.width * 0.75, colour);
    }
    response
}

/// The line over a stale page: `Offline copy from 5 min ago`, then ` — <error>` when its
/// refresh failed.
pub fn stale_line(reply: &PageReply, now_ms: i64) -> String {
    let age = age_text(now_ms.saturating_sub(reply.fetched_at_ms));
    let mut line = if age == JUST_NOW {
        format!("Offline copy from {JUST_NOW}")
    } else {
        format!("Offline copy from {age} ago")
    };
    if let Some(error) = &reply.error {
        line.push_str(" — ");
        line.push_str(error);
    }
    line
}

const JUST_NOW: &str = "just now";

/// How long ago `ms` milliseconds is, in words: `just now`, `5 min`, `3 h`, `2 days`.
pub fn age_text(ms: i64) -> String {
    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    if ms < MIN {
        JUST_NOW.to_string()
    } else if ms < HOUR {
        format!("{} min", ms / MIN)
    } else if ms < DAY {
        format!("{} h", ms / HOUR)
    } else if ms < 2 * DAY {
        "1 day".to_string()
    } else {
        format!("{} days", ms / DAY)
    }
}

/// Now, in Unix milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use egui_kittest::{Harness, kittest::Queryable};
    use engine_ipc::Page;
    use spot_core::{model::RootEntry, pages::Card};

    use super::*;

    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const TIMEOUT: &str = "Spotify didn't answer in 15 s.";

    /// A link that records what it is sent.
    #[derive(Default)]
    struct FakeLink {
        sent: RefCell<Vec<Request>>,
        last_id: Cell<RequestId>,
    }

    impl Requests for FakeLink {
        fn send(&self, req: Request) -> RequestId {
            self.sent.borrow_mut().push(req);
            self.last_id.set(self.last_id.get() + 1);
            self.last_id.get()
        }
    }

    impl FakeLink {
        /// The requests sent since the last call.
        fn take(&self) -> Vec<Request> {
            self.sent.borrow_mut().drain(..).collect()
        }

        /// The id of the request sent last.
        fn last_id(&self) -> RequestId {
            self.last_id.get()
        }
    }

    fn page(req: PageRequest) -> Request {
        Request::Page {
            req,
            prefetch: false,
        }
    }

    fn album() -> PageRequest {
        PageRequest::Album {
            uri: "spotify:album:a".into(),
        }
    }

    fn reply(fetched_at_ms: i64, stale: bool, error: Option<&str>) -> PageReply {
        PageReply {
            page: Page::Recents(Vec::<Card>::new()),
            fetched_at_ms,
            stale,
            error: error.map(Into::into),
        }
    }

    fn sidebar_reply() -> PageReply {
        PageReply {
            page: Page::Sidebar(Vec::<RootEntry>::new()),
            fetched_at_ms: 1,
            stale: false,
            error: None,
        }
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// Pages, connected, with `link` having sent nothing yet.
    fn connected(link: &FakeLink) -> Pages {
        let mut pages = Pages::new();
        pages.on_connected(link, &PageRequest::Home);
        link.take();
        pages
    }

    #[test]
    fn age_text_cases() {
        assert_eq!(age_text(0), "just now");
        assert_eq!(age_text(59_999), "just now");
        assert_eq!(age_text(-5 * MIN), "just now", "a clock that went back");
        assert_eq!(age_text(MIN), "1 min");
        assert_eq!(age_text(5 * MIN + 30_000), "5 min");
        assert_eq!(age_text(3 * HOUR), "3 h");
        assert_eq!(age_text(DAY - 1), "23 h");
        assert_eq!(age_text(DAY), "1 day");
        assert_eq!(age_text(2 * DAY + 5 * HOUR), "2 days");
    }

    #[test]
    fn stale_line_text() {
        let now = 10 * DAY;
        assert_eq!(
            stale_line(&reply(now - 5 * MIN, true, None), now),
            "Offline copy from 5 min ago"
        );
        assert_eq!(
            stale_line(&reply(now - 3 * HOUR, true, Some(TIMEOUT)), now),
            "Offline copy from 3 h ago — Spotify didn't answer in 15 s."
        );
        // Not "from just now ago".
        assert_eq!(
            stale_line(&reply(now - 1000, true, None), now),
            "Offline copy from just now"
        );
    }

    /// Final review I4: the pages that failed, or whose last refresh failed, are asked
    /// for again (the session got ready); one loaded fine, or already being asked for,
    /// isn't. A fresh copy after a failure says Spotify answers again, once.
    #[test]
    fn failing_pages_are_asked_for_again() {
        let link = FakeLink::default();
        let mut pages = connected(&link);
        let home = PageRequest::Home;
        let sidebar = PageRequest::Sidebar;
        // Recents is fine; then Home failed, and the album's refresh failed, its copy on
        // screen.
        pages.on_event(PageRequest::Recents, reply(1, false, None));
        pages.retry(&link, &home);
        let id = link.last_id();
        pages.on_reply(id, Err("not logged in".into()));
        pages.on_event(album(), reply(1, true, Some("not logged in")));
        link.take();
        assert!(!pages.take_recovered(), "nothing fresh since");

        pages.refresh_failing(&link, &[home.clone(), album(), PageRequest::Recents]);
        assert_eq!(link.take(), [page(home.clone()), page(album())]);
        assert!(matches!(pages.get(&home), Some(PageState::Loading { .. })));
        assert!(matches!(pages.get(&album()), Some(PageState::Ready(_))));
        // Asked for already: not again.
        pages.refresh_failing(&link, &[home.clone(), album()]);
        assert_eq!(link.take(), []);

        // A fresh copy of a page, after the failures: once.
        pages.on_event(sidebar.clone(), sidebar_reply());
        assert!(pages.take_recovered());
        assert!(!pages.take_recovered());
        // A stale copy isn't news of that kind.
        pages.on_event(album(), reply(1, true, Some("not logged in")));
        pages.on_event(album(), reply(1, true, None));
        assert!(!pages.take_recovered());
    }

    #[test]
    fn ensure_sends_once() {
        let link = FakeLink::default();
        let mut pages = connected(&link);
        pages.ensure(&link, &album());
        pages.ensure(&link, &album());
        assert_eq!(link.take(), vec![page(album())]);
        assert!(matches!(
            pages.get(&album()),
            Some(PageState::Loading { .. })
        ));

        let id = link_id_of(&pages, &album());
        pages.on_reply(id, Ok(Reply::Page(reply(5, false, None))));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(5, false, None)))
        );
        assert!(!pages.wants(id));
        pages.ensure(&link, &album());
        assert_eq!(
            link.take(),
            vec![],
            "a page the window has isn't asked again"
        );

        // A search with no words isn't a page to ask for.
        let empty = PageRequest::Search { query: " ".into() };
        assert!(!Pages::fetches(&empty));
        assert!(Pages::fetches(&PageRequest::Search {
            query: "jane".into()
        }));
        pages.ensure(&link, &empty);
        assert_eq!(link.take(), vec![]);
        assert_eq!(pages.get(&empty), None);
    }

    /// The id of the request out for `req`.
    fn link_id_of(pages: &Pages, req: &PageRequest) -> RequestId {
        *pages
            .pending
            .iter()
            .find(|(_, p)| &p.req == req)
            .expect("no request out")
            .0
    }

    #[test]
    fn engine_error_fails_the_page_and_retry_asks_again() {
        let link = FakeLink::default();
        let mut pages = connected(&link);
        pages.ensure(&link, &album());
        pages.on_reply(link.last_id(), Err(TIMEOUT.into()));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Failed(TIMEOUT.into()))
        );
        link.take();

        pages.ensure(&link, &album());
        assert_eq!(link.take(), vec![], "Failed waits for Retry");
        pages.retry(&link, &album());
        assert_eq!(link.take(), vec![page(album())]);
        assert!(matches!(
            pages.get(&album()),
            Some(PageState::Loading { .. })
        ));
    }

    /// Carry 1: no requests while disconnected (the link would fail them at once); a
    /// request the lost connection failed stays Loading; `Connected` asks for the current
    /// page and the sidebar.
    #[test]
    fn reconnect_asks_again() {
        let link = FakeLink::default();
        let mut pages = Pages::new();
        // Before the first connection: Loading, nothing sent.
        pages.ensure(&link, &PageRequest::Home);
        pages.ensure(&link, &PageRequest::Sidebar);
        assert_eq!(link.take(), vec![]);
        assert!(matches!(
            pages.get(&PageRequest::Home),
            Some(PageState::Loading { .. })
        ));

        pages.on_connected(&link, &PageRequest::Home);
        assert_eq!(
            link.take(),
            vec![page(PageRequest::Home), page(PageRequest::Sidebar)]
        );
        let sidebar_id = link_id_of(&pages, &PageRequest::Sidebar);
        pages.on_reply(sidebar_id, Ok(Reply::Page(sidebar_reply())));

        // Album is asked for; the engine goes before answering.
        pages.ensure(&link, &album());
        let album_id = link.last_id();
        link.take();
        pages.on_reply(album_id, Err(IpcError::Closed.to_string()));
        pages.on_disconnected();
        assert!(
            matches!(pages.get(&album()), Some(PageState::Loading { .. })),
            "{:?}",
            pages.get(&album())
        );
        // While disconnected nothing is sent, and a request the link refused is no
        // failure either.
        let other = PageRequest::Playlist {
            uri: "spotify:playlist:p".into(),
        };
        pages.ensure(&link, &other);
        assert_eq!(link.take(), vec![]);
        pages.retry(&link, &album());
        assert_eq!(link.take(), vec![]);

        pages.on_connected(&link, &album());
        // The current page, and the sidebar though it is loaded (the engine restarted).
        assert_eq!(link.take(), vec![page(album()), page(PageRequest::Sidebar)]);
        assert_eq!(
            pages.get(&PageRequest::Sidebar),
            Some(&PageState::Ready(sidebar_reply())),
            "a loaded page stays on screen while it is asked again"
        );
        // The page that wasn't current is asked for when next opened.
        assert_eq!(pages.get(&other), None);
        pages.ensure(&link, &other);
        assert_eq!(link.take(), vec![page(other)]);
    }

    /// A request made before the window heard of the disconnect fails with
    /// NOT_CONNECTED: no failure shown, and it is asked again after `Connected`.
    #[test]
    fn not_connected_is_not_a_failure() {
        let link = FakeLink::default();
        let mut pages = connected(&link);
        pages.ensure(&link, &album());
        pages.on_reply(link.last_id(), Err(NOT_CONNECTED.into()));
        assert!(matches!(
            pages.get(&album()),
            Some(PageState::Loading { .. })
        ));
        pages.on_disconnected();
        // An engine error from before the disconnect is shown, then tried afresh.
        let failed = PageRequest::Liked;
        pages
            .states
            .insert(failed.clone(), PageState::Failed(TIMEOUT.into()));
        link.take();
        pages.on_connected(&link, &album());
        assert_eq!(link.take(), vec![page(album()), page(PageRequest::Sidebar)]);
        assert_eq!(pages.get(&failed), None);
    }

    /// Carry 2: the engine carries on past its 15 s limit, and announces the page when
    /// it lands; it replaces the failure.
    #[test]
    fn late_update_replaces_failed() {
        let link = FakeLink::default();
        let mut pages = connected(&link);
        pages.ensure(&link, &album());
        pages.on_reply(link.last_id(), Err(TIMEOUT.into()));
        pages.on_event(album(), reply(7, false, None));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(7, false, None)))
        );
    }

    /// Carry 2: a page announced that the window never opened (hover prefetch) is kept,
    /// so opening it is instant and asks nothing.
    #[test]
    fn unopened_update_is_kept() {
        let link = FakeLink::default();
        let mut pages = connected(&link);
        pages.on_event(album(), reply(7, true, None));
        pages.ensure(&link, &album());
        assert_eq!(link.take(), vec![]);
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(7, true, None)))
        );
    }

    /// Carry 3: a refused prefetch leaves the page as it was; a prefetch that came back
    /// is kept.
    #[test]
    fn dropped_prefetch_leaves_page_untouched() {
        let link = FakeLink::default();
        let mut pages = connected(&link);
        pages.prefetch(&link, &album());
        pages.prefetch(&link, &album());
        assert_eq!(
            link.take(),
            vec![Request::Page {
                req: album(),
                prefetch: true
            }],
            "one prefetch at a time"
        );
        let id = link_id_of(&pages, &album());
        assert!(pages.wants(id));
        pages.on_reply(id, Ok(Reply::Dropped));
        assert_eq!(pages.get(&album()), None);
        assert!(!pages.wants(id));

        // A failed prefetch is no failure either.
        pages.prefetch(&link, &album());
        pages.on_reply(link.last_id(), Err(TIMEOUT.into()));
        assert_eq!(pages.get(&album()), None);

        // And one that came back is the page.
        pages.prefetch(&link, &album());
        pages.on_reply(link.last_id(), Ok(Reply::Page(reply(3, false, None))));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(3, false, None)))
        );
        link.take();
        pages.prefetch(&link, &album());
        assert_eq!(link.take(), vec![], "no prefetch for a page the window has");

        // A dropped answer to the window's own request leaves its Loading too.
        let liked = PageRequest::Liked;
        pages.ensure(&link, &liked);
        pages.on_reply(link.last_id(), Ok(Reply::Dropped));
        assert!(matches!(pages.get(&liked), Some(PageState::Loading { .. })));
    }

    /// An older copy never replaces a newer one, nor a stale copy the fresh one it
    /// matches; an error about a page on screen keeps the page.
    #[test]
    fn newer_copy_wins() {
        let link = FakeLink::default();
        let mut pages = connected(&link);
        pages.on_event(album(), reply(20, false, None));
        pages.on_event(album(), reply(10, true, None));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(20, false, None)))
        );
        // The same copy, stale: says nothing new.
        pages.on_event(album(), reply(20, true, None));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(20, false, None)))
        );
        // The same copy, with why its refresh failed: shown.
        pages.on_event(album(), reply(20, true, Some(TIMEOUT)));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(20, true, Some(TIMEOUT))))
        );
        pages.retry(&link, &album());
        pages.on_reply(link.last_id(), Ok(Reply::Page(reply(30, false, None))));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(30, false, None)))
        );

        // An error for a page on screen leaves it there.
        pages.on_connected(&link, &album());
        pages.on_reply(link_id_of(&pages, &album()), Err(TIMEOUT.into()));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(30, false, None)))
        );
    }

    /// Fix round 1: a loaded page is asked for again each time the window lands on it
    /// (the engine refreshes only when asked), once per landing, its copy kept on
    /// screen meanwhile.
    #[test]
    fn landing_refreshes_once() {
        let link = FakeLink::default();
        let mut pages = connected(&link);
        // The start page, Home, is out already: landing on it asks nothing more.
        pages.showing(&link, &PageRequest::Home);
        assert_eq!(link.take(), vec![]);

        pages.showing(&link, &album());
        assert_eq!(link.take(), vec![page(album())]);
        pages.on_reply(link.last_id(), Ok(Reply::Page(reply(5, false, None))));
        for _ in 0..3 {
            pages.showing(&link, &album());
        }
        assert_eq!(link.take(), vec![], "once per landing, not per frame");

        pages.showing(&link, &PageRequest::Home);
        assert_eq!(link.take(), vec![]);
        pages.showing(&link, &album());
        pages.showing(&link, &album());
        assert_eq!(link.take(), vec![page(album())], "came back: one refresh");
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(5, false, None))),
            "the copy stays on screen while it is refreshed"
        );
        // The engine's cached copy comes back at once, then the fresh one.
        let refresh = link.last_id();
        pages.on_reply(refresh, Ok(Reply::Page(reply(5, true, None))));
        pages.on_event(album(), reply(9, false, None));
        assert_eq!(
            pages.get(&album()),
            Some(&PageState::Ready(reply(9, false, None)))
        );

        // Coming back to a failed page asks for it again too.
        pages.showing(&link, &PageRequest::Liked);
        pages.on_reply(link.last_id(), Err(TIMEOUT.into()));
        pages.showing(&link, &album());
        link.take();
        pages.showing(&link, &PageRequest::Liked);
        assert_eq!(link.take(), vec![page(PageRequest::Liked)]);
        assert!(matches!(
            pages.get(&PageRequest::Liked),
            Some(PageState::Loading { .. })
        ));

        // While disconnected nothing is sent; Connected asks for the current page.
        pages.on_disconnected();
        pages.showing(&link, &album());
        assert_eq!(link.take(), vec![]);
        pages.on_connected(&link, &album());
        assert_eq!(link.take(), vec![page(album()), page(PageRequest::Sidebar)]);
        pages.showing(&link, &album());
        assert_eq!(link.take(), vec![]);
    }

    /// Draws `state` and records a Retry click.
    struct Shown {
        state: PageState,
        retried: bool,
    }

    fn harness(state: PageState) -> Harness<'static, Shown> {
        Harness::new_ui_state(
            |ui, shown: &mut Shown| {
                show(
                    ui,
                    &shown.state,
                    || shown.retried = true,
                    |ui, _| {
                        ui.label("the body");
                    },
                );
            },
            Shown {
                state,
                retried: false,
            },
        )
    }

    #[test]
    fn failed_page_shows_retry() {
        let mut harness = harness(PageState::Failed(TIMEOUT.into()));
        harness.run();
        harness.get_by_label(WARNING);
        harness.get_by_label(TIMEOUT);
        assert!(harness.query_by_label("the body").is_none());
        assert!(!harness.state().retried);
        harness.get_by_label("Retry").click();
        harness.run();
        assert!(harness.state().retried);
    }

    #[test]
    fn stale_page_shows_age() {
        let mut harness = harness(PageState::Ready(reply(
            now_ms() - 5 * MIN,
            true,
            Some(TIMEOUT),
        )));
        harness.run();
        harness.get_by_label("Offline copy from 5 min ago — Spotify didn't answer in 15 s.");
        harness.get_by_label("the body");

        harness.state_mut().state = PageState::Ready(reply(now_ms() - 3 * HOUR, true, None));
        harness.run();
        harness.get_by_label("Offline copy from 3 h ago");

        // A fresh page has no such line.
        harness.state_mut().state = PageState::Ready(reply(now_ms(), false, None));
        harness.run();
        assert!(harness.query_by_label_contains("Offline copy").is_none());
        harness.get_by_label("the body");
    }

    #[test]
    fn loading_page_is_never_blank() {
        let mut harness = harness(PageState::Loading {
            since: Instant::now(),
        });
        harness.run();
        harness.get_by_label(LOADING);
        assert!(harness.query_by_label("the body").is_none());
        assert!(harness.query_by_label(RETRY).is_none());
    }
}
