use std::time::{Duration, Instant};

use eframe::egui::{
    self, Event, Modifiers, PointerButton, Rect, Stroke, Vec2,
    accesskit::{Role, Toggled},
    pos2, vec2,
};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use engine_ipc::{Page, PageReply, PageRequest, PlayerState, Reply, Request};
use spot_core::pages::{
    Card, CardKind, Home, Named, PlaylistPage, SearchResults, Section, TrackRow, parse_album,
    parse_artist, parse_home, parse_liked, parse_playlist, parse_recents, parse_search,
};

use super::*;
use crate::{
    art::Art,
    page_frame::{Pages, RETRY},
    test_support::{self, FakeLink, fills, fixture, outlines},
    theme::{self, THUMB},
    track_list::{HEART_LABEL, RowAction},
    view_state::{PanelTab, ViewState},
};

const USER: &str = "dannycake";
const ALBUM: &str = "spotify:album:21b4cDNse2AMpj94ykfuON";
const JANE: &str = "spotify:artist:2rLGlNI6htigNxx172qxLu";
const MS: Duration = Duration::from_millis(1);

fn reply_at(page: Page, fetched_at_ms: i64) -> PageReply {
    PageReply {
        page,
        fetched_at_ms,
        stale: false,
        error: None,
    }
}

fn reply(page: Page) -> PageReply {
    reply_at(page, 1)
}

fn page(req: PageRequest) -> Request {
    Request::Page {
        req,
        prefetch: false,
    }
}

fn prefetch(req: PageRequest) -> Request {
    Request::Page {
        req,
        prefetch: true,
    }
}

fn album_req() -> PageRequest {
    PageRequest::Album { uri: ALBUM.into() }
}

fn playlist_req() -> PageRequest {
    PageRequest::Playlist {
        uri: "spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi".into(),
    }
}

fn search_req(query: &str) -> PageRequest {
    PageRequest::Search {
        query: query.into(),
    }
}

fn home_page() -> Home {
    parse_home(&fixture("home.json")).unwrap()
}

/// The window around the pages, as the shell holds it.
struct Shown {
    pages: Pages,
    views: PageViews,
    view: ViewState,
    art: Option<Art>,
    link: FakeLink,
    player: PlayerState,
    username: Option<String>,
    now: Instant,
}

impl Shown {
    /// On `req`, connected, its request out and nothing else sent yet.
    fn new(req: PageRequest) -> Shown {
        let mut view = ViewState::default();
        view.nav.go(req.clone());
        let link = FakeLink::default();
        let mut pages = Pages::new();
        pages.on_connected(&link, &req);
        link.take();
        Shown {
            pages,
            views: PageViews::default(),
            view,
            art: None,
            link,
            player: idle_player(),
            username: Some(USER.into()),
            now: Instant::now(),
        }
    }

    /// On `req`, whose page is in.
    fn with(req: PageRequest, page: Page) -> Shown {
        let mut shown = Shown::new(req.clone());
        shown.pages.on_event(req, reply(page));
        shown
    }

    fn current(&self) -> &PageRequest {
        self.view.nav.current()
    }

    /// What was sent since the last call, LikedState requests left out.
    fn sent(&self) -> Vec<Request> {
        self.link
            .take()
            .into_iter()
            .filter(|r| !matches!(r, Request::LikedState { .. }))
            .collect()
    }
}

fn harness(shown: Shown) -> Harness<'static, Shown> {
    let (builder, _) = test_support::builder(vec2(1200.0, 800.0), false);
    let mut harness = builder.build_ui_state(
        |ui, s: &mut Shown| {
            let art = s.art.get_or_insert_with(|| Art::offline(ui.ctx().clone()));
            let env = Env {
                link: &s.link,
                view: &mut s.view,
                art,
                player: &s.player,
                username: s.username.as_deref(),
                now: s.now,
            };
            show(ui, &mut s.pages, &mut s.views, env);
        },
        shown,
    );
    harness.run();
    harness
}

/// The track row titled `title`.
fn row_rect(harness: &Harness<'_, Shown>, title: &str) -> Rect {
    harness
        .query_all_by_role_and_label(Role::ListItem, title)
        .next()
        .unwrap_or_else(|| panic!("no row {title}"))
        .rect()
}

/// A double-click on the row titled `title`, in its title column, in one frame.
fn double_click(harness: &mut Harness<'_, Shown>, title: &str) {
    let rect = row_rect(harness, title);
    let pos = pos2(rect.left() + 160.0, rect.center().y);
    let events = &mut harness.input_mut().events;
    events.push(Event::PointerMoved(pos));
    for _ in 0..2 {
        for pressed in [true, false] {
            events.push(Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            });
        }
    }
    harness.run();
}

/// The label (not a row's link of the same name) reading `text`.
fn label(harness: &Harness<'_, Shown>, text: &str) {
    let n = harness
        .query_all_by_role_and_label(Role::Label, text)
        .count();
    assert_eq!(n, 1, "labels reading {text}");
}

fn card_button<'h>(harness: &'h Harness<'_, Shown>, label: &'h str) -> egui_kittest::Node<'h> {
    harness
        .query_all_by_role_and_label(Role::Button, label)
        .next()
        .unwrap_or_else(|| panic!("no card {label}"))
}

/// The cover thumbnails drawn last frame: squares `THUMB` wide in the grey of a cover
/// still loading (the test's covers never load).
fn thumbs(harness: &Harness<'_, Shown>) -> usize {
    fills(harness)
        .into_iter()
        .filter(|(r, c)| *c == theme::LINES && r.width() == THUMB && r.height() == THUMB)
        .count()
}

fn hearts(harness: &Harness<'_, Shown>) -> Vec<Option<Toggled>> {
    harness
        .query_all_by_label(HEART_LABEL)
        .map(|n| n.accesskit_node().toggled())
        .collect()
}

fn play_context(context: &str, row: &TrackRow) -> Request {
    Request::PlayContext {
        context_uri: context.into(),
        track_uri: Some(row.uri.clone()),
        track_uid: row.uid.clone(),
        shuffle: None,
    }
}

fn card(uri: &str, kind: CardKind, title: &str) -> Card {
    Card {
        uri: uri.into(),
        kind,
        title: title.into(),
        subtitle: String::new(),
        images: vec![],
    }
}

fn row(uri: &str, title: &str) -> TrackRow {
    TrackRow {
        uri: uri.into(),
        uid: None,
        title: title.into(),
        artists: vec![Named {
            uri: JANE.into(),
            name: "Jane Remover".into(),
        }],
        album: None,
        duration_ms: 1000,
        images: vec![],
        added_at: None,
    }
}

// ---- Search: debounce, stale replies, the field ----

#[test]
fn debounce_waits_150ms() {
    let t0 = Instant::now();
    let mut d = Debounce::default();
    assert_eq!(d.due(t0), None, "nothing typed");
    assert_eq!(d.wait(t0), None);
    for (ms, text) in [(0, "j"), (50, "ja"), (100, "jan")] {
        d.typed(text, t0 + ms * MS);
        assert_eq!(d.due(t0 + ms * MS), None, "{ms} ms");
    }
    assert_eq!(d.wait(t0 + 100 * MS), Some(DEBOUNCE));
    assert_eq!(d.wait(t0 + 200 * MS), Some(50 * MS));
    assert_eq!(d.due(t0 + 200 * MS), None);
    assert_eq!(d.due(t0 + 249 * MS), None);
    assert_eq!(d.due(t0 + 250 * MS), Some("jan".to_string()));
    // Once.
    assert_eq!(d.due(t0 + 400 * MS), None);
    assert_eq!(d.wait(t0 + 400 * MS), None);
}

/// Task 26 ruling (final pass): while typing, the results of the last query sent stay
/// on screen until the next query's arrive, with no `Loading…` per keystroke; a reply
/// for a query sent before the last one is never shown.
#[test]
fn search_keeps_the_last_results_until_new_ones_arrive() {
    let results = parse_search(&fixture("search.json")).unwrap();
    let first = results.tracks[0].title.clone();
    let census = SearchResults {
        tracks: vec![row("spotify:track:census", "Census")],
        artists: vec![],
        albums: vec![],
        playlists: vec![],
    };
    let mut harness = harness(Shown::new(search_req("")));
    assert_eq!(harness.state().sent(), vec![], "no words, no search");

    harness.get_by_role(Role::TextInput).type_text("jane");
    harness.run();
    assert_eq!(harness.state().sent(), vec![], "not before 150 ms");
    harness.state_mut().now += DEBOUNCE;
    harness.run();
    assert_eq!(harness.state().sent(), vec![page(search_req("jane"))]);
    assert_eq!(harness.state().current(), &search_req("jane"));
    let jane_id = harness.state().link.last_id();
    label(&harness, LOADING);

    // More words typed, not sent yet: `jane`'s results, the last sent, show as they come.
    harness.get_by_role(Role::TextInput).type_text(" remover");
    harness.run();
    let s = harness.state_mut();
    let jane = Reply::Page(reply(Page::Search(results.clone())));
    s.pages.on_reply(jane_id, Ok(jane));
    harness.run();
    row_rect(&harness, &first);

    // Sent: they stay until the new ones are in.
    harness.state_mut().now += DEBOUNCE;
    harness.run();
    assert_eq!(
        harness.state().sent(),
        vec![page(search_req("jane remover"))]
    );
    row_rect(&harness, &first);
    assert!(
        harness.query_by_label(LOADING).is_none(),
        "no Loading… flash"
    );
    let id = harness.state().link.last_id();
    let s = harness.state_mut();
    s.pages
        .on_reply(id, Ok(Reply::Page(reply(Page::Search(census.clone())))));
    harness.run();
    row_rect(&harness, "Census");
    assert!(
        harness
            .query_all_by_role_and_label(Role::ListItem, &first)
            .next()
            .is_none()
    );
    // One history entry for the search, whatever was typed.
    assert_eq!(harness.state().current(), &search_req("jane remover"));

    // Two queries sent before either answers: the older one's reply isn't shown.
    harness.get_by_role(Role::TextInput).type_text(" x");
    harness.run();
    harness.state_mut().now += DEBOUNCE;
    harness.run();
    let older = harness.state().link.last_id();
    harness.get_by_role(Role::TextInput).type_text("y");
    harness.run();
    harness.state_mut().now += DEBOUNCE;
    harness.run();
    let newer = harness.state().link.last_id();
    assert_eq!(
        harness.state().sent(),
        vec![
            page(search_req("jane remover x")),
            page(search_req("jane remover xy"))
        ]
    );
    let s = harness.state_mut();
    s.pages
        .on_reply(older, Ok(Reply::Page(reply(Page::Search(results.clone())))));
    harness.run();
    assert!(
        harness
            .query_all_by_role_and_label(Role::ListItem, &first)
            .next()
            .is_none(),
        "the older query's results"
    );
    row_rect(&harness, "Census");
    let s = harness.state_mut();
    s.pages
        .on_reply(newer, Ok(Reply::Page(reply(Page::Search(results.clone())))));
    harness.run();
    row_rect(&harness, &first);

    assert!(harness.state_mut().view.nav.back());
    assert_eq!(harness.state().current(), &PageRequest::Home);

    // Search results play alone.
    assert!(harness.state_mut().view.nav.forward());
    harness.run();
    double_click(&mut harness, &first);
    assert_eq!(
        harness.state().sent(),
        vec![Request::PlayUri {
            uri: results.tracks[0].uri.clone()
        }]
    );
}

/// Carry 7: the field is focused on landing, outlined 1 px in the accent (Look 2), its
/// corners softened; selected text in it is black on the green.
#[test]
fn search_field_focused_with_accent_outline() {
    let harness = harness(Shown::new(search_req("")));
    let field = harness.get_by_role(Role::TextInput);
    assert!(field.is_focused());
    let rect = field.rect();
    let around: Vec<Stroke> = outlines(&harness)
        .into_iter()
        .filter(|(r, _)| r.expand(2.0).contains_rect(rect) && rect.expand(2.0).contains_rect(*r))
        .map(|(_, s)| s)
        .collect();
    assert_eq!(around, vec![theme::FOCUS]);
    assert_eq!(
        crate::theme::visuals().selection.stroke,
        Stroke::new(1.0, theme::ON_ACCENT)
    );
    let field_fill = fills(&harness)
        .into_iter()
        .find(|(r, _)| r.expand(2.0).contains_rect(rect) && rect.expand(2.0).contains_rect(*r))
        .map(|(_, c)| c);
    assert_eq!(field_fill, Some(theme::SURFACE));
}

/// Opening a search (back to it, say) puts its words in the field.
#[test]
fn search_opens_with_its_words() {
    let results = parse_search(&fixture("search.json")).unwrap();
    let harness = harness(Shown::with(search_req("jane"), Page::Search(results)));
    assert_eq!(
        harness.get_by_role(Role::TextInput).value().as_deref(),
        Some("jane")
    );
    label(&harness, "Tracks");
}

// ---- Home, Recents and cards ----

#[test]
fn home_renders_sections() {
    let harness = harness(Shown::with(PageRequest::Home, Page::Home(home_page())));
    harness.get_by_label("Good evening");
    harness.get_by_label("Jump back in");
    harness.get_by_label("Made For dannycake");
    card_button(&harness, "PRETTY SURGEON");
}

/// Carry 5: podcasts and other kinds with no page aren't shown.
#[test]
fn other_cards_hidden() {
    let home = Home {
        greeting: "Hi".into(),
        sections: vec![
            Section {
                title: "Mixed".into(),
                cards: vec![
                    card("spotify:show:s", CardKind::Other, "A Podcast"),
                    card(ALBUM, CardKind::Album, "Revengeseekerz"),
                ],
            },
            Section {
                title: "Podcasts only".into(),
                cards: vec![card("spotify:show:t", CardKind::Other, "Another")],
            },
        ],
    };
    let harness = harness(Shown::with(PageRequest::Home, Page::Home(home)));
    card_button(&harness, "Revengeseekerz");
    assert!(harness.query_by_label("A Podcast").is_none());
    assert!(harness.query_by_label("Another").is_none());
    assert!(
        harness.query_by_label("Podcasts only").is_none(),
        "a section left with no cards"
    );
}

/// Carry 5: what a card opens and plays.
#[test]
fn card_targets() {
    let album = card(ALBUM, CardKind::Album, "a");
    assert_eq!(card_page(&album), Some(album_req()));
    let list = card("spotify:playlist:p", CardKind::Playlist, "p");
    assert_eq!(
        card_page(&list),
        Some(PageRequest::Playlist {
            uri: "spotify:playlist:p".into()
        })
    );
    let artist = card(JANE, CardKind::Artist, "j");
    assert_eq!(
        card_page(&artist),
        Some(PageRequest::Artist { uri: JANE.into() })
    );
    let track = card("spotify:track:t", CardKind::Track, "t");
    assert_eq!(card_page(&track), None);
    let other = card("spotify:show:s", CardKind::Other, "s");
    assert_eq!(card_page(&other), None);
    let liked = card(LIKED_SONGS_URI, CardKind::Playlist, "Liked Songs");
    assert_eq!(card_page(&liked), Some(PageRequest::Liked));

    let context = |uri: &str| {
        Some(Request::PlayContext {
            context_uri: uri.into(),
            track_uri: None,
            track_uid: None,
            shuffle: None,
        })
    };
    for c in [&album, &list, &artist] {
        assert_eq!(card_play(c, Some(USER)), context(&c.uri));
    }
    assert_eq!(
        card_play(&track, Some(USER)),
        Some(Request::PlayUri {
            uri: "spotify:track:t".into()
        })
    );
    assert_eq!(card_play(&other, Some(USER)), None);
    assert_eq!(
        card_play(&liked, Some(USER)),
        context("spotify:user:dannycake:collection")
    );
    assert_eq!(card_play(&liked, None), None, "no account name yet");
}

/// A click opens the card's page; its play button plays it.
#[test]
fn cards_open_and_play() {
    let mut harness = harness(Shown::with(PageRequest::Home, Page::Home(home_page())));
    card_button(&harness, "PRETTY SURGEON").click();
    harness.run();
    assert_eq!(
        harness.state().current(),
        &PageRequest::Album {
            uri: "spotify:album:6jV98kIvbHjhRiIeakALqF".into()
        }
    );
    assert!(harness.state_mut().view.nav.back());
    harness.run();

    card_button(&harness, "Liked Songs").click();
    harness.run();
    assert_eq!(harness.state().current(), &PageRequest::Liked);
    assert!(harness.state_mut().view.nav.back());
    harness.run();
    harness.state().link.take();

    card_button(&harness, "Play PRETTY SURGEON").click();
    harness.run();
    assert_eq!(harness.state().current(), &PageRequest::Home);
    assert_eq!(
        harness.state().sent(),
        vec![Request::PlayContext {
            context_uri: "spotify:album:6jV98kIvbHjhRiIeakALqF".into(),
            track_uri: None,
            track_uid: None,
            shuffle: None,
        }]
    );
    card_button(&harness, "Play Liked Songs").click();
    harness.run();
    assert_eq!(
        harness.state().sent(),
        vec![Request::PlayContext {
            context_uri: "spotify:user:dannycake:collection".into(),
            track_uri: None,
            track_uid: None,
            shuffle: None,
        }]
    );
}

/// Look 2: under the pointer a card shows Spotify's play button, the green with a black
/// triangle, a third bigger than before, on the hover grey with softened corners.
#[test]
fn card_play_button_look() {
    let mut harness = harness(Shown::with(PageRequest::Home, Page::Home(home_page())));
    let card = card_button(&harness, "PRETTY SURGEON").rect();
    harness.event(Event::PointerMoved(card.center()));
    harness.run();
    let discs: Vec<_> = test_support::circles(&harness)
        .into_iter()
        .filter(|(c, _, _)| card.contains(*c))
        .collect();
    assert_eq!(discs.len(), 1, "{discs:?}");
    assert_eq!(discs[0].1, theme::PLAY_BUTTON / 2.0);
    assert_eq!(discs[0].2, theme::ACCENT);
    let disc = Rect::from_center_size(discs[0].0, Vec2::splat(theme::PLAY_BUTTON));
    let marks = test_support::marks_within(&harness, disc);
    assert!(marks.contains(&theme::ON_ACCENT), "{marks:?}");
    assert!(fills(&harness).contains(&(card, theme::HOVER)));
}

/// Recently played: cards; a track's plays, Liked Songs' opens its page.
#[test]
fn recents_cards() {
    let cards = parse_recents(&fixture("recents.json")).unwrap();
    let nerve = cards[1].uri.clone();
    let mut harness = harness(Shown::with(PageRequest::Recents, Page::Recents(cards)));
    harness.get_by_label("Recently played");
    card_button(&harness, "nerve").click();
    harness.run();
    assert_eq!(
        harness.state().sent(),
        vec![Request::PlayUri { uri: nerve }]
    );
    assert_eq!(harness.state().current(), &PageRequest::Recents);
    card_button(&harness, "Liked Songs").click();
    harness.run();
    assert_eq!(harness.state().current(), &PageRequest::Liked);
}

// ---- Hover prefetch ----

#[test]
fn hover_prefetch_once() {
    let t0 = Instant::now();
    let a = album_req();
    let b = PageRequest::Artist { uri: JANE.into() };
    let mut h = HoverPrefetch::default();
    assert_eq!(h.hover(Some(a.clone()), t0), None);
    assert_eq!(h.wait(t0), Some(HOVER));
    assert_eq!(h.hover(Some(a.clone()), t0 + 100 * MS), None);
    assert_eq!(h.wait(t0 + 100 * MS), Some(50 * MS));
    assert_eq!(h.hover(Some(a.clone()), t0 + 150 * MS), Some(a.clone()));
    assert_eq!(h.wait(t0 + 150 * MS), None);
    assert_eq!(h.hover(Some(a.clone()), t0 + 300 * MS), None, "once");
    // Away and back: still once.
    assert_eq!(h.hover(None, t0 + 400 * MS), None);
    assert_eq!(h.hover(Some(a.clone()), t0 + 500 * MS), None);
    assert_eq!(h.hover(Some(a.clone()), t0 + 900 * MS), None);
    // Another target starts its own wait; passing over one doesn't count.
    assert_eq!(h.hover(Some(b.clone()), t0 + 1000 * MS), None);
    assert_eq!(h.hover(None, t0 + 1100 * MS), None);
    assert_eq!(h.hover(Some(b.clone()), t0 + 1200 * MS), None);
    assert_eq!(h.hover(Some(b.clone()), t0 + 1349 * MS), None);
    assert_eq!(h.hover(Some(b.clone()), t0 + 1350 * MS), Some(b));
}

/// A card hovered for 150 ms has its page prefetched, once.
#[test]
fn hovered_card_prefetches_its_page() {
    let mut harness = harness(Shown::with(PageRequest::Home, Page::Home(home_page())));
    card_button(&harness, "PRETTY SURGEON").hover();
    harness.run();
    assert_eq!(harness.state().sent(), vec![]);
    harness.state_mut().now += HOVER;
    harness.run();
    let surgeon = PageRequest::Album {
        uri: "spotify:album:6jV98kIvbHjhRiIeakALqF".into(),
    };
    assert_eq!(harness.state().sent(), vec![prefetch(surgeon)]);
    harness.state_mut().now += 10 * HOVER;
    harness.run();
    assert_eq!(harness.state().sent(), vec![]);
}

#[test]
fn hover_prefetch_targets() {
    assert!(prefetches_on_hover(&album_req()));
    assert!(prefetches_on_hover(&PageRequest::Artist {
        uri: JANE.into()
    }));
    assert!(prefetches_on_hover(&playlist_req()));
    for not in [
        PageRequest::Liked,
        PageRequest::Playlist {
            uri: LIKED_SONGS_URI.into(),
        },
        PageRequest::Home,
        PageRequest::Recents,
        PageRequest::Sidebar,
        search_req("jane"),
    ] {
        assert!(!prefetches_on_hover(&not), "{not:?}");
    }
}

/// Fix round 1: hovering Liked Songs' card prefetches nothing: its page is the whole
/// library. An album's card still prefetches (`hovered_card_prefetches_its_page`).
#[test]
fn liked_songs_card_never_prefetched() {
    let mut harness = harness(Shown::with(PageRequest::Home, Page::Home(home_page())));
    card_button(&harness, "Liked Songs").hover();
    harness.run();
    harness.state_mut().now += HOVER;
    harness.run();
    harness.state_mut().now += 10 * HOVER;
    harness.run();
    assert_eq!(harness.state().sent(), vec![]);
}

/// Carry 10: a row's artist link and the header's, hovered for 150 ms, prefetch the
/// artist's page, once.
#[test]
fn hovered_links_prefetch_once() {
    let album = parse_album(&fixture("album.json")).unwrap();
    let mut harness = harness(Shown::with(album_req(), Page::Album(album)));
    harness.state().link.take();
    let links: Vec<Rect> = harness
        .query_all_by_role_and_label(Role::Link, "Jane Remover")
        .map(|n| n.rect())
        .collect();
    assert!(links.len() > 1, "the header's link and the rows'");
    let artist = PageRequest::Artist { uri: JANE.into() };
    for rect in [links[0], links[1], links[0]] {
        harness.event(Event::PointerMoved(rect.center()));
        harness.run();
        harness.state_mut().now += HOVER;
        harness.run();
    }
    assert_eq!(harness.state().sent(), vec![prefetch(artist)]);
}

/// The header's artist link has the shared link look: white, underlined 1 px under the
/// pointer.
#[test]
fn header_link_looks_like_a_link() {
    let album = parse_album(&fixture("album.json")).unwrap();
    let mut harness = harness(Shown::with(album_req(), Page::Album(album)));
    let header = harness
        .query_all_by_role_and_label(Role::Link, "Jane Remover")
        .next()
        .unwrap()
        .rect();
    let colour = |h: &Harness<'_, Shown>| {
        test_support::texts(h)
            .into_iter()
            .find(|(t, _, r)| t == "Jane Remover" && header.contains(r.center()))
            .map(|(_, c, _)| c)
    };
    let underlines = |h: &Harness<'_, Shown>| {
        let mut out = vec![];
        for clipped in &h.output().shapes {
            if let egui::Shape::LineSegment { points, stroke } = &clipped.shape
                && *stroke == Stroke::new(1.0, theme::TEXT)
                && header.x_range().contains(points[0].x)
            {
                out.push(*points);
            }
        }
        out
    };
    assert_eq!(colour(&harness), Some(theme::TEXT));
    assert!(underlines(&harness).is_empty());
    harness.event(Event::PointerMoved(header.center()));
    harness.run();
    assert_eq!(colour(&harness), Some(theme::TEXT));
    let under = underlines(&harness);
    assert_eq!(under.len(), 1);
    assert_eq!(under[0][0].y, header.bottom() - 0.5);
}

// ---- Album, playlist, Liked Songs, artist ----

/// Album rows leave their covers out; they play in the album's context.
#[test]
fn album_has_no_thumbs() {
    let album = parse_album(&fixture("album.json")).unwrap();
    let first = album.tracks[0].clone();
    let mut harness = harness(Shown::with(album_req(), Page::Album(album)));
    label(&harness, "Revengeseekerz");
    label(&harness, "2025 · deadAir");
    row_rect(&harness, &first.title);
    assert_eq!(thumbs(&harness), 0);
    double_click(&mut harness, &first.title);
    assert_eq!(harness.state().sent(), vec![play_context(ALBUM, &first)]);

    // The header's artist link opens the artist.
    harness
        .query_all_by_role_and_label(Role::Link, "Jane Remover")
        .next()
        .unwrap()
        .click();
    harness.run();
    assert_eq!(
        harness.state().current(),
        &PageRequest::Artist { uri: JANE.into() }
    );
}

/// Playlist rows show their covers; they play in the playlist's context.
#[test]
fn playlist_has_thumbs() {
    let list = parse_playlist(&fixture("playlist.json")).unwrap();
    let uri = list.uri.clone();
    let first = list.tracks[0].clone();
    let n = list.tracks.len();
    let mut harness = harness(Shown::with(playlist_req(), Page::Playlist(list)));
    label(&harness, "Main");
    label(&harness, "81 songs");
    assert_eq!(thumbs(&harness), n);
    double_click(&mut harness, &first.title);
    assert_eq!(harness.state().sent(), vec![play_context(&uri, &first)]);
}

/// Carry 6: a description's tags and entities aren't shown as such.
#[test]
fn playlist_description_is_plain() {
    let list = PlaylistPage {
        uri: "spotify:playlist:p".into(),
        name: "Mixes".into(),
        description: "Mixed &amp; <a href=\"spotify:user:x\">matched</a> &#x2F; more".into(),
        images: vec![],
        total: 1,
        tracks: vec![row("spotify:track:a", "Alpha")],
    };
    let req = PageRequest::Playlist {
        uri: "spotify:playlist:p".into(),
    };
    let harness = harness(Shown::with(req, Page::Playlist(list)));
    harness.get_by_label("Mixed & matched / more");
    harness.get_by_label("1 song");
}

#[test]
fn plain_text_cases() {
    assert_eq!(plain_text("plain"), "plain");
    assert_eq!(plain_text("a &amp; b"), "a & b");
    assert_eq!(plain_text("x &#x2F; y &#47; z"), "x / y / z");
    assert_eq!(
        plain_text("&lt;3 &gt; &quot;q&quot; &#39;s &apos;t"),
        "<3 > \"q\" 's 't"
    );
    assert_eq!(
        plain_text("Hear <a href=\"spotify:artist:x\">Jane</a> live"),
        "Hear Jane live"
    );
    assert_eq!(plain_text("one<br>two<br/>three"), "one two three");
    assert_eq!(plain_text("  spaced \n out  "), "spaced out");
    // What isn't an entity or a tag stays.
    assert_eq!(plain_text("R&B & more"), "R&B & more");
    assert_eq!(plain_text("&bogus; &#xZZ;"), "&bogus; &#xZZ;");
    assert_eq!(plain_text("1 < 2"), "1 < 2");
    assert_eq!(plain_text("&#x1F3B5;"), "\u{1F3B5}");
    // Not ASCII after `&` or `<`: kept, and no slicing inside a character.
    assert_eq!(plain_text("&ééééééééééé; <é"), "&ééééééééééé; <é");
}

/// Liked Songs: the total in the header; rows liked by definition, playing in the
/// account's collection.
#[test]
fn liked_songs_page() {
    let liked = parse_liked(&fixture("liked_tracks.json")).unwrap();
    let first = liked.tracks[0].clone();
    let n = liked.tracks.len();
    let mut harness = harness(Shown::with(PageRequest::Liked, Page::Liked(liked)));
    harness.get_by_label("Liked Songs");
    harness.get_by_label("758 songs");
    assert_eq!(thumbs(&harness), n);
    assert_eq!(hearts(&harness), vec![Some(Toggled::True); n]);
    assert_eq!(
        harness.state().link.take(),
        vec![],
        "no LikedState for Liked Songs"
    );
    double_click(&mut harness, &first.title);
    assert_eq!(
        harness.state().sent(),
        vec![play_context("spotify:user:dannycake:collection", &first)]
    );
    // Unliked elsewhere: the engine says so.
    harness
        .state_mut()
        .views
        .liked_set
        .set(first.uri.clone(), false);
    harness.run();
    assert_eq!(hearts(&harness)[0], Some(Toggled::False));
}

/// Fix round 1: a Liked Songs copy never hearts again a track the engine said was
/// unliked: not a cached copy that still holds it, not after a reconnect, and not on
/// the other pages that show it.
#[test]
fn liked_copy_does_not_reheart_an_unliked_track() {
    let liked = parse_liked(&fixture("liked_tracks.json")).unwrap();
    let x = liked.tracks[0].uri.clone();
    let n = liked.tracks.len();
    let mut harness = harness(Shown::with(PageRequest::Liked, Page::Liked(liked.clone())));
    assert_eq!(hearts(&harness), vec![Some(Toggled::True); n]);
    // Liked Songs' copies don't speak for the other pages: the engine does.
    assert_eq!(harness.state().views.liked_set.get(&x), None);
    // Unliked: the engine says so.
    harness.state_mut().views.liked_set.set(x.clone(), false);
    harness.run();
    assert_eq!(hearts(&harness)[0], Some(Toggled::False));

    // A cached copy still holding the track: its rows are drawn liked, it isn't.
    let mut stale = reply_at(Page::Liked(liked.clone()), 2);
    stale.stale = true;
    harness
        .state_mut()
        .pages
        .on_event(PageRequest::Liked, stale);
    harness.run();
    assert_eq!(harness.state().views.liked_set.get(&x), Some(false));
    let mut drawn = vec![Some(Toggled::True); n];
    drawn[0] = Some(Toggled::False);
    assert_eq!(hearts(&harness), drawn);

    // A reconnect asks each page afresh; the copy on screen is drawn again.
    harness.state_mut().views.on_connected();
    harness.run();
    assert_eq!(harness.state().views.liked_set.get(&x), Some(false));

    // The fresh copy, without the track.
    let mut fresh = liked.clone();
    fresh.tracks.remove(0);
    harness
        .state_mut()
        .pages
        .on_event(PageRequest::Liked, reply_at(Page::Liked(fresh), 3));
    harness.run();
    assert_eq!(harness.state().views.liked_set.get(&x), Some(false));
    // The rest are still drawn liked.
    assert_eq!(hearts(&harness), vec![Some(Toggled::True); n - 1]);
}

#[test]
fn artist_page() {
    let artist = parse_artist(&fixture("artist.json")).unwrap();
    let top = artist.top_tracks[0].clone();
    let req = PageRequest::Artist { uri: JANE.into() };
    let mut harness = harness(Shown::with(req, Page::Artist(artist)));
    label(&harness, "Jane Remover");
    for section in ["Popular", "Albums", "Singles", "Fans also like"] {
        harness.get_by_label(section);
    }
    card_button(&harness, "Revengeseekerz");
    card_button(&harness, "underscores");
    harness.get_by_label("Start radio").click();
    harness.run();
    assert_eq!(
        harness.state().sent(),
        vec![Request::StartRadio {
            seed_uri: JANE.into()
        }]
    );
    double_click(&mut harness, &top.title);
    assert_eq!(harness.state().sent(), vec![play_context(JANE, &top)]);
}

// ---- Carries: failure, hearts, scroll, actions ----

/// Carry 8: a page that failed (pathfinder's 403) says why, and Retry asks again.
#[test]
fn failed_page_retries() {
    let mut harness = harness(Shown::new(album_req()));
    harness.get_by_label(LOADING);
    // The first request out: the album's.
    let s = harness.state_mut();
    s.pages
        .on_reply(1, Err("Spotify answered HTTP 403.".into()));
    harness.run();
    harness.get_by_label("Spotify answered HTTP 403.");
    harness.get_by_label(RETRY).click();
    harness.run();
    assert_eq!(harness.state().sent(), vec![page(album_req())]);
}

/// Carry 4: one LikedState per page reply; its answers fill the hearts.
#[test]
fn liked_state_once_per_reply() {
    let list = parse_playlist(&fixture("playlist.json")).unwrap();
    let uris: Vec<String> = list.tracks.iter().map(|r| r.uri.clone()).collect();
    let mut harness = harness(Shown::with(playlist_req(), Page::Playlist(list.clone())));
    assert_eq!(
        harness.state().link.take(),
        vec![Request::LikedState { uris: uris.clone() }]
    );
    let id = harness.state().link.last_id();
    assert_eq!(hearts(&harness), vec![Some(Toggled::False); 3], "unknown");
    harness.run();
    assert_eq!(harness.state().link.take(), vec![], "once");

    let s = harness.state_mut();
    assert!(s.views.wants(id));
    s.views.on_reply(
        id,
        Ok(Reply::LikedState(vec![Some(true), Some(false), Some(true)])),
    );
    harness.run();
    assert_eq!(
        hearts(&harness),
        vec![
            Some(Toggled::True),
            Some(Toggled::False),
            Some(Toggled::True)
        ]
    );

    // A fresh copy of the page is asked about again.
    let s = harness.state_mut();
    s.pages
        .on_event(playlist_req(), reply_at(Page::Playlist(list), 2));
    harness.run();
    assert_eq!(
        harness.state().link.take(),
        vec![Request::LikedState { uris }]
    );
}

/// Final review C1: an engine that hasn't read Liked Songs yet answers unknown, which
/// leaves a heart as the window last knew it (drawn as not liked when never known); once
/// it has (`LikedSetReady`) the page is asked about again and its hearts corrected. A
/// heart asks for the state it doesn't show: one drawn empty never unlikes.
#[test]
fn unknown_hearts_are_asked_about_again_once_the_engine_knows() {
    let list = parse_playlist(&fixture("playlist.json")).unwrap();
    let uris: Vec<String> = list.tracks.iter().map(|r| r.uri.clone()).collect();
    let mut shown = Shown::with(playlist_req(), Page::Playlist(list.clone()));
    // What the window knew from before an engine restart.
    shown.views.liked_set.set(uris[1].clone(), true);
    shown.views.on_connected();
    let mut harness = harness(shown);
    assert_eq!(
        harness.state().link.take(),
        vec![Request::LikedState { uris: uris.clone() }]
    );
    let id = harness.state().link.last_id();
    harness
        .state_mut()
        .views
        .on_reply(id, Ok(Reply::LikedState(vec![Some(false), None, None])));
    harness.run();
    assert_eq!(
        hearts(&harness),
        vec![
            Some(Toggled::False),
            Some(Toggled::True),
            Some(Toggled::False)
        ],
        "unknown keeps what was known, else not liked"
    );
    assert_eq!(harness.state().link.take(), vec![], "asked once");

    // The engine has read Liked Songs: the page is asked about again.
    harness.state_mut().views.liked_set.on_liked_set_ready();
    harness.run();
    assert_eq!(
        harness.state().link.take(),
        vec![Request::LikedState { uris: uris.clone() }]
    );
    let id = harness.state().link.last_id();
    harness.state_mut().views.on_reply(
        id,
        Ok(Reply::LikedState(vec![
            Some(false),
            Some(false),
            Some(true),
        ])),
    );
    harness.run();
    assert_eq!(
        hearts(&harness),
        vec![
            Some(Toggled::False),
            Some(Toggled::False),
            Some(Toggled::True)
        ]
    );
    // The third, liked, unlikes; the first, not liked, likes.
    harness
        .query_all_by_label(HEART_LABEL)
        .nth(2)
        .unwrap()
        .click();
    harness.run();
    harness
        .query_all_by_label(HEART_LABEL)
        .next()
        .unwrap()
        .click();
    harness.run();
    assert_eq!(
        harness.state().link.take(),
        vec![
            Request::SetLiked {
                uri: uris[2].clone(),
                liked: false
            },
            Request::SetLiked {
                uri: uris[0].clone(),
                liked: true
            },
        ]
    );
}

/// Carries 2 and 3: a page's list is shown from its saved offset once its rows are in,
/// and keeps the offset up to date; a loading page leaves it alone.
#[test]
fn list_scroll_saved_per_page() {
    let tracks: Vec<TrackRow> = (0..300)
        .map(|i| row(&format!("spotify:track:s{i}"), &format!("Song {i}")))
        .collect();
    let list = PlaylistPage {
        uri: "spotify:playlist:p".into(),
        name: "Long".into(),
        description: String::new(),
        images: vec![],
        total: 300,
        tracks,
    };
    let req = PageRequest::Playlist {
        uri: "spotify:playlist:p".into(),
    };
    let mut shown = Shown::new(req.clone());
    shown.view.set_scroll(&req, 100.0 * theme::ROW_HEIGHT);
    let mut harness = harness(shown);
    harness.get_by_label(LOADING);
    assert_eq!(harness.state().view.scroll_of(&req), 4000.0);

    harness
        .state_mut()
        .pages
        .on_event(req.clone(), reply(Page::Playlist(list)));
    harness.run();
    row_rect(&harness, "Song 100");
    assert!(harness.query_by_label("Song 0").is_none());
    assert_eq!(harness.state().view.scroll_of(&req), 4000.0);
}

/// Carry 1: every row action does what it says; carry 11: a link double-clicked
/// (GoArtist twice) adds one page to the history.
#[test]
fn row_actions() {
    let link = FakeLink::default();
    let mut view = ViewState::default();
    let mut views = PageViews::default();
    let mut art = Art::offline(egui::Context::default());
    let player = idle_player();
    let rows = vec![row("spotify:track:a", "Alpha"), {
        let mut b = row("spotify:track:b", "Beta");
        b.uid = Some("ub".into());
        b
    }];
    let list = "spotify:playlist:p";
    {
        let ViewState {
            nav, scroll, panel, ..
        } = &mut view;
        let mut cx = PageCx {
            link: &link,
            nav,
            page: PageRequest::Home,
            scroll,
            panel,
            art: &mut art,
            player: &player,
            username: Some(USER),
            selection: &mut views.selection,
            liked_set: &mut views.liked_set,
            credits: &mut views.credits,
            hovered: None,
            now: Instant::now(),
            sent: &mut views.sent,
        };
        let a = || "spotify:track:a".to_string();
        for action in [
            RowAction::Play { index: 1 },
            RowAction::Heart {
                uri: a(),
                liked: true,
            },
            RowAction::AddToQueue(a()),
            RowAction::StartRadio(a()),
            RowAction::CopyLink("https://open.spotify.com/track/a".into()),
            RowAction::ShowCredits(a()),
            RowAction::GoAlbum(ALBUM.into()),
            RowAction::GoArtist(JANE.into()),
            RowAction::GoArtist(JANE.into()),
        ] {
            row_action(action, &rows, Some(list), &mut cx);
        }
        row_action(RowAction::Play { index: 0 }, &rows, None, &mut cx);
        row_action(RowAction::Play { index: 9 }, &rows, Some(list), &mut cx);
    }
    assert_eq!(
        link.take(),
        vec![
            play_context(list, &rows[1]),
            Request::SetLiked {
                uri: "spotify:track:a".into(),
                liked: true
            },
            Request::AddToQueue {
                uri: "spotify:track:a".into()
            },
            Request::StartRadio {
                seed_uri: "spotify:track:a".into()
            },
            Request::PlayUri {
                uri: "spotify:track:a".into()
            },
        ]
    );
    assert_eq!(view.panel, Some(PanelTab::Credits));
    assert_eq!(views.credits.as_deref(), Some("spotify:track:a"));
    assert_eq!(
        view.nav.current(),
        &PageRequest::Artist { uri: JANE.into() }
    );
    assert!(view.nav.back());
    assert_eq!(view.nav.current(), &album_req(), "the artist went in once");

    // A failed request is the pages' to report, for the notices (Task 29 carry 3); a
    // heart's answer fills the set.
    let heart = 2;
    assert!(views.wants(heart));
    let liked = views.on_reply(
        heart,
        Ok(Reply::Liked {
            uri: "spotify:track:a".into(),
            liked: true,
        }),
    );
    assert_eq!(liked, None);
    assert_eq!(views.liked_set.get("spotify:track:a"), Some(true));
    assert!(!views.wants(heart));
    assert!(views.wants(1));
    assert_eq!(
        views.on_reply(1, Err("The player is restarting.".into())),
        Some("Couldn't play: The player is restarting.".into())
    );
    assert!(!views.wants(1));
    assert_eq!(
        views.on_reply(3, Err("Spotify didn't answer.".into())),
        Some("Couldn't add to the queue: Spotify didn't answer.".into())
    );
    // The engine's own words for a heart are kept whole.
    assert_eq!(
        views.on_reply(2, Err("Couldn't like it: no.".into())),
        None,
        "answered already"
    );
    assert_eq!(views.on_reply(4, Ok(Reply::Ok)), None);
}

#[test]
fn album_meta_line() {
    assert_eq!(album::meta_line("2025-04-04", "deadAir"), "2025 · deadAir");
    assert_eq!(album::meta_line("2003", ""), "2003");
    assert_eq!(album::meta_line("", "Label"), "Label");
    assert_eq!(album::meta_line("", " "), "");
}

#[test]
fn banner_crops_to_fill() {
    use eframe::egui::Vec2;
    let wide = artist::cover_uv(Vec2::new(2000.0, 500.0), Vec2::new(1000.0, 500.0));
    assert_eq!(
        (wide.min.x, wide.max.x, wide.min.y, wide.max.y),
        (0.25, 0.75, 0.0, 1.0)
    );
    let tall = artist::cover_uv(Vec2::new(1000.0, 1000.0), Vec2::new(1000.0, 250.0));
    assert_eq!(
        (tall.min.x, tall.max.x, tall.min.y, tall.max.y),
        (0.0, 1.0, 0.375, 0.625)
    );
    let none = artist::cover_uv(Vec2::ZERO, Vec2::new(1.0, 1.0));
    assert_eq!(none, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)));
}

/// Liked Songs playing under the name Recently played gives it still marks its row.
#[test]
fn liked_context_alias() {
    let mut player = idle_player();
    player.context_uri = Some(LIKED_SONGS_URI.into());
    let ctx = "spotify:user:dannycake:collection";
    assert_eq!(liked_player(&player, ctx).context_uri.as_deref(), Some(ctx));
    player.context_uri = Some("spotify:album:x".into());
    assert!(matches!(liked_player(&player, ctx), Cow::Borrowed(_)));
}
