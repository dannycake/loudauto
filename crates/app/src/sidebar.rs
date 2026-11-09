//! The left sidebar: Home, Search, Recently played and Liked Songs, then the owner's
//! playlists from the rootlist (`PageRequest::Sidebar`), a folder as a secondary header
//! with its playlists indented under it. The current page's entry has the quiet accent
//! fill and a 3 px accent stripe down its left edge; keyboard focus adds a 1 px accent
//! outline. The footer holds the gear that opens the settings, marked the same way while
//! they are open.

use std::{f32::consts::TAU, sync::Arc, time::Instant};

use eframe::egui::{
    self, Color32, Galley, Painter, Pos2, Rect, Response, Role, ScrollArea, Sense, Stroke,
    StrokeKind, TextStyle, TextWrapMode, Ui, WidgetInfo, WidgetText, pos2, vec2,
};
use engine_ipc::{Page, PageRequest};
use spot_core::model::RootEntry;

use crate::{
    nav::page_name,
    page_frame::{self, PageState},
    theme,
};

/// The sidebar's width, in points.
pub const WIDTH: f32 = 240.0;
/// An entry's height, in points.
const ENTRY_HEIGHT: f32 = 32.0;
/// Text's distance from the sidebar's left edge, in points.
const PAD: f32 = 16.0;
/// How far each folder level indents its playlists, in points.
const INDENT: f32 = 16.0;
/// The gear's entry in the footer.
pub const SETTINGS: &str = "Settings";

/// The entries above the playlists, in order.
pub const FIXED: [PageRequest; 4] = [
    PageRequest::Home,
    PageRequest::Search {
        query: String::new(),
    },
    PageRequest::Recents,
    PageRequest::Liked,
];

/// What the owner did in the sidebar.
#[derive(Debug, Clone, PartialEq)]
pub enum SidebarAction {
    /// Open this page.
    Go(PageRequest),
    /// Ask for the playlists again.
    Retry,
    /// Open the settings, or close them.
    Settings,
}

/// A line of the playlists: a playlist, or a folder's header; `depth` folders deep.
#[derive(Debug, Clone, PartialEq)]
enum Row<'a> {
    Playlist {
        uri: &'a str,
        name: &'a str,
        depth: usize,
    },
    Folder {
        name: &'a str,
        depth: usize,
    },
}

/// The rootlist as lines, folders flattened.
fn rows(entries: &[RootEntry]) -> Vec<Row<'_>> {
    let mut depth = 0;
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry {
            RootEntry::Playlist(p) => rows.push(Row::Playlist {
                uri: &p.uri,
                name: &p.name,
                depth,
            }),
            RootEntry::FolderStart { name, .. } => {
                rows.push(Row::Folder { name, depth });
                depth += 1;
            }
            RootEntry::FolderEnd { .. } => depth = depth.saturating_sub(1),
            RootEntry::Other { .. } => {}
        }
    }
    rows
}

/// Whether `entry`'s entry is the current page's: any search is Search's.
pub fn is_current(entry: &PageRequest, current: &PageRequest) -> bool {
    match (entry, current) {
        (PageRequest::Search { .. }, PageRequest::Search { .. }) => true,
        _ => entry == current,
    }
}

/// Draws the sidebar into `ui`, `playlists` being the rootlist page's state. While the
/// settings are open (`settings`) the gear is the entry marked, and every page's entry,
/// the current one's too, says Go.
pub fn show(
    ui: &mut Ui,
    current: &PageRequest,
    settings: bool,
    playlists: Option<&PageState>,
) -> Option<SidebarAction> {
    let mut action = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    // First, so the playlists scroll in what is left above it.
    let gear = egui::Panel::bottom("sidebar-footer")
        .resizable(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin {
            top: 8,
            ..egui::Margin::ZERO
        }))
        .show(ui, |ui| settings_entry(ui, settings))
        .inner;
    if gear.clicked() {
        action = Some(SidebarAction::Settings);
    }
    for fixed in &FIXED {
        let selected = !settings && is_current(fixed, current);
        // The current page's entry does nothing: Search's keeps the search's words.
        if entry(ui, page_name(fixed), 0, selected).clicked() && !selected {
            action = Some(SidebarAction::Go(fixed.clone()));
        }
    }
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(8.0);
    let loading = PageState::Loading {
        since: Instant::now(),
    };
    let state = playlists.unwrap_or(&loading);
    ScrollArea::vertical()
        .id_salt("sidebar-playlists")
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(PAD);
                ui.vertical(|ui| {
                    let retry = || action = Some(SidebarAction::Retry);
                    // The body is drawn below, at full width.
                    page_frame::show(ui, state, retry, |_, _| {});
                });
            });
            let PageState::Ready(reply) = state else {
                return;
            };
            let Page::Sidebar(entries) = &reply.page else {
                log::warn!("the sidebar's page isn't a rootlist");
                return;
            };
            for row in rows(entries) {
                match row {
                    Row::Playlist { uri, name, depth } => {
                        let selected = !settings
                            && matches!(current, PageRequest::Playlist { uri: u } if u == uri);
                        if entry(ui, name, depth, selected).clicked() {
                            action = Some(SidebarAction::Go(PageRequest::Playlist {
                                uri: uri.to_string(),
                            }));
                        }
                    }
                    Row::Folder { name, depth } => header(ui, name, depth),
                }
            }
        });
    action
}

/// The rect for one line of the sidebar, its full width.
fn line(ui: &mut Ui, sense: Sense) -> (Rect, Response) {
    ui.allocate_exact_size(vec2(ui.available_width(), ENTRY_HEIGHT), sense)
}

/// `text` truncated to fit beside `indent`, as a galley.
fn galley(ui: &Ui, text: &str, rect: Rect, indent: f32) -> Arc<Galley> {
    let width = (rect.width() - indent - PAD).max(0.0);
    WidgetText::from(text).into_galley(ui, Some(TextWrapMode::Truncate), width, TextStyle::Button)
}

/// An entry: the hover grey under the pointer; when `selected` the quiet accent fill and
/// an accent stripe down its left edge; an accent outline while it has keyboard focus.
fn entry(ui: &mut Ui, text: &str, depth: usize, selected: bool) -> Response {
    let (rect, response) = line(ui, Sense::click());
    response.widget_info(|| WidgetInfo::selected(Role::Button, ui.is_enabled(), selected, text));
    if ui.is_rect_visible(rect) {
        let colour = paint_entry(ui, &response, rect, selected);
        draw_text(ui, text, rect, indent(depth), colour);
    }
    response
}

/// The footer's entry: a gear and `Settings`, marked as the current page's is while the
/// settings are `open`.
fn settings_entry(ui: &mut Ui, open: bool) -> Response {
    let (rect, response) = line(ui, Sense::click());
    response.widget_info(|| WidgetInfo::selected(Role::Button, ui.is_enabled(), open, SETTINGS));
    if ui.is_rect_visible(rect) {
        let colour = paint_entry(ui, &response, rect, open);
        let side = theme::ICON;
        let centre = pos2(rect.left() + PAD + side / 2.0, rect.center().y);
        gear(ui.painter(), centre, side / 2.0, colour);
        draw_text(ui, SETTINGS, rect, PAD + side + PAD / 2.0, colour);
    }
    response
}

/// An entry's back: the selected or hover fill, the stripe and the focus outline, as
/// `response` and `selected` call for; the colour of its text, white.
fn paint_entry(ui: &Ui, response: &Response, rect: Rect, selected: bool) -> Color32 {
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 0.0, theme::SELECTED);
        let stripe = Rect::from_min_size(rect.min, vec2(theme::STRIPE_WIDTH, rect.height()));
        painter.rect_filled(stripe, 0.0, theme::ACCENT);
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, theme::HOVER);
    }
    if response.has_focus() {
        painter.rect_stroke(rect, 0.0, theme::FOCUS, StrokeKind::Inside);
    }
    theme::TEXT
}

/// A gear, drawn rather than set (the fonts may lack U+2699): a ring with eight teeth
/// and a hub, `radius` big, in thin lines.
fn gear(painter: &Painter, centre: Pos2, radius: f32, colour: Color32) {
    let ring = radius * 0.62;
    painter.circle_stroke(centre, ring, Stroke::new(1.5, colour));
    painter.circle_stroke(centre, radius * 0.22, Stroke::new(1.0, colour));
    for i in 0..8 {
        let angle = i as f32 * TAU / 8.0;
        let dir = vec2(angle.cos(), angle.sin());
        let tooth = [centre + dir * ring, centre + dir * radius];
        painter.line_segment(tooth, Stroke::new(2.0, colour));
    }
}

/// A folder's name: secondary, and not an entry.
fn header(ui: &mut Ui, text: &str, depth: usize) {
    let (rect, response) = line(ui, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, ui.is_enabled(), text));
    if ui.is_rect_visible(rect) {
        draw_text(ui, text, rect, indent(depth), theme::SECONDARY);
    }
}

/// Text's distance from the left edge `depth` folders deep.
fn indent(depth: usize) -> f32 {
    PAD + INDENT * depth as f32
}

fn draw_text(ui: &Ui, text: &str, rect: Rect, indent: f32, colour: Color32) {
    let galley = galley(ui, text, rect, indent);
    let pos = pos2(
        rect.left() + indent,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(pos, galley, colour);
}

#[cfg(test)]
mod tests {
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };
    use spot_core::model::PlaylistRef;

    use eframe::egui;
    use engine_ipc::PageReply;

    use super::*;
    use crate::test_support;

    fn playlist(uri: &str, name: &str) -> RootEntry {
        RootEntry::Playlist(PlaylistRef {
            uri: uri.into(),
            name: name.into(),
        })
    }

    fn rootlist() -> PageState {
        PageState::Ready(PageReply {
            page: Page::Sidebar(vec![
                playlist("spotify:playlist:a", "Morning"),
                RootEntry::FolderStart {
                    id: "f".into(),
                    name: "Jane Remover".into(),
                },
                playlist("spotify:playlist:b", "Census Designated"),
                RootEntry::FolderStart {
                    id: "g".into(),
                    name: "Deeper".into(),
                },
                playlist("spotify:playlist:c", "Frailty"),
                RootEntry::FolderEnd { id: "g".into() },
                RootEntry::FolderEnd { id: "f".into() },
                RootEntry::Other {
                    uri: "spotify:x".into(),
                },
                playlist("spotify:playlist:d", "Night"),
            ]),
            fetched_at_ms: 1,
            stale: false,
            error: None,
        })
    }

    /// The sidebar, drawn for `current` with `playlists` (the settings open when
    /// `settings`); records what it returned.
    struct Shown {
        current: PageRequest,
        settings: bool,
        playlists: Option<PageState>,
        action: Option<SidebarAction>,
    }

    fn harness(current: PageRequest, playlists: Option<PageState>) -> Harness<'static, Shown> {
        Harness::new_ui_state(
            |ui, shown: &mut Shown| {
                let action = show(ui, &shown.current, shown.settings, shown.playlists.as_ref());
                if let Some(action) = action {
                    shown.action = Some(action);
                }
            },
            Shown {
                current,
                settings: false,
                playlists,
                action: None,
            },
        )
    }

    #[test]
    fn current_entry() {
        let search = |q: &str| PageRequest::Search { query: q.into() };
        assert!(is_current(&FIXED[1], &search("jane")));
        assert!(is_current(&FIXED[1], &search("")));
        assert!(is_current(&PageRequest::Home, &PageRequest::Home));
        assert!(!is_current(&PageRequest::Home, &PageRequest::Liked));
        let p = |uri: &str| PageRequest::Playlist { uri: uri.into() };
        assert!(is_current(
            &p("spotify:playlist:a"),
            &p("spotify:playlist:a")
        ));
        assert!(!is_current(
            &p("spotify:playlist:a"),
            &p("spotify:playlist:b")
        ));
    }

    /// Folders nest; a folder's end ends its indent; what isn't a playlist or a folder
    /// isn't shown.
    #[test]
    fn rootlist_rows() {
        let PageState::Ready(reply) = rootlist() else {
            unreachable!()
        };
        let Page::Sidebar(entries) = &reply.page else {
            unreachable!()
        };
        assert_eq!(
            rows(entries),
            vec![
                Row::Playlist {
                    uri: "spotify:playlist:a",
                    name: "Morning",
                    depth: 0
                },
                Row::Folder {
                    name: "Jane Remover",
                    depth: 0
                },
                Row::Playlist {
                    uri: "spotify:playlist:b",
                    name: "Census Designated",
                    depth: 1
                },
                Row::Folder {
                    name: "Deeper",
                    depth: 1
                },
                Row::Playlist {
                    uri: "spotify:playlist:c",
                    name: "Frailty",
                    depth: 2
                },
                Row::Playlist {
                    uri: "spotify:playlist:d",
                    name: "Night",
                    depth: 0
                },
            ]
        );
        // An end with no start doesn't go below the top level.
        let odd = [
            RootEntry::FolderEnd { id: "x".into() },
            playlist("spotify:playlist:a", "Morning"),
        ];
        assert_eq!(
            rows(&odd),
            vec![Row::Playlist {
                uri: "spotify:playlist:a",
                name: "Morning",
                depth: 0
            }]
        );
    }

    fn selected(harness: &Harness<'_, Shown>, label: &str) -> bool {
        use egui::accesskit::Toggled;
        harness.get_by_label(label).accesskit_node().toggled() == Some(Toggled::True)
    }

    #[test]
    fn sidebar_entries_and_folders() {
        let current = PageRequest::Playlist {
            uri: "spotify:playlist:b".into(),
        };
        let mut harness = harness(current, Some(rootlist()));
        harness.run();
        for fixed in ["Home", "Search", "Recently played", "Liked Songs"] {
            assert!(!selected(&harness, fixed), "{fixed}");
        }
        assert!(selected(&harness, "Census Designated"));
        for other in ["Morning", "Frailty", "Night"] {
            assert!(!selected(&harness, other), "{other}");
        }
        // The fixed entries come first, in order.
        let top = |label: &str| harness.get_by_label(label).rect().top();
        assert!(top("Home") < top("Search"));
        assert!(top("Search") < top("Recently played"));
        assert!(top("Recently played") < top("Liked Songs"));
        assert!(top("Liked Songs") < top("Morning"));

        // A folder is a header, not an entry, above its playlists.
        let role = |label: &str| harness.get_by_label(label).accesskit_node().role();
        assert_eq!(role("Jane Remover"), Role::Label);
        assert_eq!(role("Census Designated"), Role::Button);
        assert!(top("Morning") < top("Jane Remover"));
        assert!(top("Jane Remover") < top("Census Designated"));
        assert!(top("Frailty") < top("Night"));

        harness.get_by_label("Night").click();
        harness.run();
        assert_eq!(
            harness.state().action,
            Some(SidebarAction::Go(PageRequest::Playlist {
                uri: "spotify:playlist:d".into()
            }))
        );
        harness.get_by_label("Liked Songs").click();
        harness.run();
        assert_eq!(
            harness.state().action,
            Some(SidebarAction::Go(PageRequest::Liked))
        );
    }

    #[test]
    fn current_fixed_entry_is_marked() {
        let mut harness = harness(
            PageRequest::Search {
                query: "jane".into(),
            },
            None,
        );
        harness.run();
        assert!(selected(&harness, "Search"));
        assert!(!selected(&harness, "Home"));
        // With no rootlist yet, the playlists say they are loading.
        harness.get_by_label(page_frame::LOADING);
    }

    /// Look 2: the current page's entry is the quiet accent fill with the accent stripe
    /// down its left edge and white words; a folder's name is secondary; the gear is a
    /// third bigger than a line of text.
    #[test]
    fn current_entry_look() {
        let mut harness = harness(PageRequest::Home, Some(rootlist()));
        harness.run();
        let home = harness.get_by_label("Home").rect();
        let fills = test_support::fills(&harness);
        assert!(fills.contains(&(home, theme::SELECTED)), "{fills:?}");
        let stripe = Rect::from_min_size(home.min, vec2(theme::STRIPE_WIDTH, home.height()));
        let accents: Vec<Rect> = fills
            .iter()
            .filter(|(_, c)| *c == theme::ACCENT)
            .map(|(r, _)| *r)
            .collect();
        assert_eq!(accents, [stripe]);
        let colour = |text: &str| {
            test_support::texts(&harness)
                .into_iter()
                .find(|(t, _, _)| t == text)
                .map(|(_, c, _)| c)
        };
        assert_eq!(colour("Home"), Some(theme::TEXT));
        assert_eq!(colour("Search"), Some(theme::TEXT));
        assert_eq!(colour("Jane Remover"), Some(theme::SECONDARY));
        let gear = test_support::drawn_within(&harness, harness.get_by_label(SETTINGS).rect());
        assert!((gear.width() - theme::ICON).abs() <= 2.5, "{gear:?}");
    }

    /// Carry from Task 23: Search clicked while searching keeps the words; elsewhere it
    /// opens an empty search.
    #[test]
    fn search_entry_keeps_the_query() {
        let jane = PageRequest::Search {
            query: "jane".into(),
        };
        let mut searching = harness(jane, None);
        searching.run();
        searching.get_by_label("Search").click();
        searching.run();
        assert_eq!(searching.state().action, None);

        let mut home = harness(PageRequest::Home, None);
        home.run();
        home.get_by_label("Search").click();
        home.run();
        assert_eq!(
            home.state().action,
            Some(SidebarAction::Go(PageRequest::Search {
                query: String::new()
            }))
        );
    }

    /// The gear in the footer, below the playlists, opens the settings; while they are
    /// open it is marked, no page is, and the current page's entry goes back to it.
    #[test]
    fn settings_gear_in_footer() {
        let mut harness = harness(PageRequest::Home, Some(rootlist()));
        harness.run();
        assert!(!selected(&harness, SETTINGS));
        let top = |h: &Harness<'_, Shown>, label: &str| h.get_by_label(label).rect().top();
        assert!(top(&harness, "Night") < top(&harness, SETTINGS));
        let bottom = harness.get_by_label(SETTINGS).rect().bottom();
        assert!(bottom > harness.ctx.content_rect().bottom() - ENTRY_HEIGHT - 1.0);
        harness.get_by_label(SETTINGS).click();
        harness.run();
        assert_eq!(harness.state().action, Some(SidebarAction::Settings));

        harness.state_mut().settings = true;
        harness.state_mut().action = None;
        harness.run();
        assert!(selected(&harness, SETTINGS));
        assert!(!selected(&harness, "Home"));
        harness.get_by_label("Home").click();
        harness.run();
        assert_eq!(
            harness.state().action,
            Some(SidebarAction::Go(PageRequest::Home))
        );
    }

    #[test]
    fn playlists_failed_shows_retry() {
        let mut harness = harness(
            PageRequest::Home,
            Some(PageState::Failed("Spotify didn't answer in 15 s.".into())),
        );
        harness.run();
        assert!(selected(&harness, "Home"));
        let words = harness
            .get_by_label("Spotify didn't answer in 15 s.")
            .rect();
        test_support::assert_in_red_box(&harness, "Spotify didn't answer in 15 s.", words);
        let retry = harness.get_by_label(page_frame::RETRY).rect();
        test_support::assert_main_button(&harness, page_frame::RETRY, retry);
        harness.get_by_label(page_frame::RETRY).click();
        harness.run();
        assert_eq!(harness.state().action, Some(SidebarAction::Retry));
    }
}
