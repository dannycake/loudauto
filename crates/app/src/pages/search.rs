//! Search: a text field, focused on landing, whose words are sent 150 ms after the last
//! keystroke. While typing, the results of the last query sent stay on screen until the
//! next query's arrive (no `Loading…` per keystroke); a late reply for a query sent
//! before the last one is never shown. Results: tracks (each plays alone), artists,
//! albums and playlists.

use std::time::{Duration, Instant};

use eframe::egui::{Frame, Margin, RichText, Stroke, TextEdit, Ui};
use engine_ipc::{Page, PageReply, PageRequest};

use super::{List, PageCx, cards, framed, other_page, scrolled, track_list};
use crate::{
    page_frame::{PageState, Pages},
    theme,
};

/// How long after the last keystroke the words are sent.
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// What the empty field says.
pub const HINT: &str = "Songs, artists, albums or playlists";
/// What a search that found nothing says.
pub const NOTHING_FOUND: &str = "Nothing found.";
/// The field's width, in points.
const FIELD_WIDTH: f32 = 480.0;

/// Words typed, sent once the typing pauses for [`DEBOUNCE`].
#[derive(Debug, Default)]
pub struct Debounce {
    /// The field's words after the last keystroke, and when it was.
    pending: Option<(String, Instant)>,
}

impl Debounce {
    /// A keystroke at `now` left the field holding `text`.
    pub fn typed(&mut self, text: &str, now: Instant) {
        self.pending = Some((text.to_owned(), now));
    }

    /// The words to send, once [`DEBOUNCE`] has passed since the last keystroke; once.
    pub fn due(&mut self, now: Instant) -> Option<String> {
        let (_, at) = self.pending.as_ref()?;
        if now.saturating_duration_since(*at) < DEBOUNCE {
            return None;
        }
        self.pending.take().map(|(text, _)| text)
    }

    /// How long until the words are due.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        let (_, at) = self.pending.as_ref()?;
        let left = DEBOUNCE.saturating_sub(now.saturating_duration_since(*at));
        // Due now: the next frame sends them.
        Some(left.max(Duration::from_millis(1)))
    }
}

/// The search field and what it has sent.
#[derive(Debug, Default)]
pub struct SearchBox {
    pub text: String,
    /// The query last put to the page: the field's own, or the one the page opened with.
    query: String,
    /// The query whose results are on screen: the last sent that has its answer.
    shown: Option<String>,
    debounce: Debounce,
}

/// The search page for `query` (the history's): the field, then the results for its
/// words. `previous` is the page drawn last frame.
pub fn show(
    ui: &mut Ui,
    query: &str,
    previous: Option<&PageRequest>,
    pages: &mut Pages,
    search: &mut SearchBox,
    cx: &mut PageCx,
) {
    let id = ui.make_persistent_id("search-field");
    let landed = !matches!(previous, Some(PageRequest::Search { .. }));
    // Opened (from the sidebar, or back to a search): the field shows its words.
    if landed || query != search.query {
        search.text = query.to_owned();
        search.query = query.to_owned();
        search.shown = None;
        search.debounce = Debounce::default();
    }
    if landed {
        ui.memory_mut(|m| m.request_focus(id));
    }
    // Carry 7: focused, the field's own outline is the 1 px focus outline, not
    // `selection.stroke` (the colour of selected text, black on the green).
    let stroke = if ui.memory(|m| m.has_focus(id)) {
        theme::FOCUS
    } else {
        Stroke::new(1.0, theme::LINES)
    };
    let frame = Frame::new()
        .fill(theme::SURFACE)
        .stroke(stroke)
        .corner_radius(theme::CORNER)
        .inner_margin(Margin::symmetric(10, 6));
    let field = TextEdit::singleline(&mut search.text)
        .id(id)
        .hint_text(HINT)
        .desired_width(FIELD_WIDTH)
        .frame(frame);
    if ui.add(field).changed() {
        search.debounce.typed(&search.text, cx.now);
    }
    if let Some(text) = search.debounce.due(cx.now) {
        let words = text.trim();
        if words != search.query {
            // The search's page changes in place: one page in the history per search.
            search.query = words.to_owned();
            let req = PageRequest::Search {
                query: search.query.clone(),
            };
            cx.nav.replace(req.clone());
            pages.showing(cx.link, &req);
        }
    }
    if let Some(wait) = search.debounce.wait(cx.now) {
        ui.ctx().request_repaint_after(wait);
    }
    ui.add_space(16.0);

    if search.text.trim().is_empty() {
        search.shown = None;
        return;
    }
    // The last query sent, once it has its answer; until then the results on screen,
    // else its `Loading…`. An answer for a query sent before it is never put up.
    let sent = PageRequest::Search {
        query: search.query.clone(),
    };
    let answered = matches!(
        pages.get(&sent),
        Some(PageState::Ready(_) | PageState::Failed(_))
    );
    if answered {
        search.shown = Some(search.query.clone());
    }
    let shown = match &search.shown {
        Some(query) => PageRequest::Search {
            query: query.clone(),
        },
        None => sent,
    };
    cx.page = shown.clone();
    framed(ui, &shown, pages, cx);
}

/// A search's results: tracks, each playing alone, then artists, albums and playlists.
pub fn results(ui: &mut Ui, reply: &PageReply, cx: &mut PageCx) {
    let Page::Search(results) = &reply.page else {
        return other_page(ui);
    };
    scrolled(ui, cx, |ui, cx| {
        if !results.tracks.is_empty() {
            cards::heading(ui, "Tracks");
            let list = List {
                rows: &results.tracks,
                context: None,
                thumbs: true,
            };
            track_list(ui, reply, list, true, cx);
            ui.add_space(24.0);
        }
        cards::section(ui, "Artists", &results.artists, "artists", cx);
        cards::section(ui, "Albums", &results.albums, "albums", cx);
        cards::section(ui, "Playlists", &results.playlists, "playlists", cx);
        let empty = results.tracks.is_empty()
            && [&results.artists, &results.albums, &results.playlists]
                .iter()
                .all(|cards| cards.iter().all(|c| !cards::is_shown(c)));
        if empty {
            ui.label(RichText::new(NOTHING_FOUND).color(theme::SECONDARY));
        }
    });
}
