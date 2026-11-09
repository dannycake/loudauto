//! Where the window is: the page shown, and the pages before and after it, as a
//! browser keeps them. Alt+← and Alt+→ go back and forward (not the mouse's side
//! buttons, which the desktop turns into the keys `5` and `4`). The start page is Home.

use eframe::egui::{self, Key, Modifiers};
use engine_ipc::PageRequest;
use serde::{Deserialize, Serialize};

/// The most pages the history holds; the oldest go first. It is saved with the window,
/// so it would otherwise grow from one session to the next.
pub const HISTORY_MAX: usize = 100;

/// The pages visited, oldest first, and which of them is shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Nav {
    history: Vec<PageRequest>,
    cursor: usize,
}

impl Default for Nav {
    fn default() -> Self {
        Nav {
            history: vec![PageRequest::Home],
            cursor: 0,
        }
    }
}

impl Nav {
    /// The page shown.
    pub fn current(&self) -> &PageRequest {
        &self.history[self.cursor]
    }

    /// Opens `req`, dropping the pages forward of the current one. Opening the page
    /// already shown does nothing.
    pub fn go(&mut self, req: PageRequest) {
        if *self.current() == req {
            return;
        }
        self.history.truncate(self.cursor + 1);
        self.history.push(req);
        let excess = self.history.len().saturating_sub(HISTORY_MAX);
        self.history.drain(..excess);
        self.cursor = self.history.len() - 1;
    }

    /// Shows `req` in place of the current page, as a search does as its words change:
    /// no page is added to the history.
    pub fn replace(&mut self, req: PageRequest) {
        self.history[self.cursor] = req;
    }

    /// The pages in the history, oldest first.
    pub fn pages(&self) -> &[PageRequest] {
        &self.history
    }

    /// One page back; false at the first.
    pub fn back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// One page forward; false at the last.
    pub fn forward(&mut self) -> bool {
        if self.cursor + 1 >= self.history.len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// Alt+← and Alt+→, consumed from this frame's input: true if the page changed.
    pub fn keys(&mut self, ctx: &egui::Context) -> bool {
        let (back, forward) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::ALT, Key::ArrowLeft),
                i.consume_key(Modifiers::ALT, Key::ArrowRight),
            )
        });
        (back && self.back()) | (forward && self.forward())
    }

    /// Whether `cursor` points into `history`: false for a saved one that was edited.
    pub fn is_valid(&self) -> bool {
        self.cursor < self.history.len()
    }
}

/// What a page is called: the sidebar's words for its fixed entries.
pub fn page_name(req: &PageRequest) -> &'static str {
    match req {
        PageRequest::Home => "Home",
        PageRequest::Search { .. } => "Search",
        PageRequest::Recents => "Recently played",
        PageRequest::Liked => "Liked Songs",
        PageRequest::Artist { .. } => "Artist",
        PageRequest::Album { .. } => "Album",
        PageRequest::Playlist { .. } => "Playlist",
        PageRequest::Sidebar => "Playlists",
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::Harness;

    use super::*;

    fn album() -> PageRequest {
        PageRequest::Album {
            uri: "spotify:album:a".into(),
        }
    }

    fn artist() -> PageRequest {
        PageRequest::Artist {
            uri: "spotify:artist:b".into(),
        }
    }

    fn search() -> PageRequest {
        PageRequest::Search {
            query: "jane remover".into(),
        }
    }

    #[test]
    fn nav_back_forward() {
        let mut nav = Nav::default();
        assert_eq!(nav.current(), &PageRequest::Home);
        assert!(!nav.back(), "nothing before the start page");
        nav.go(album());
        nav.go(artist());
        assert!(nav.back());
        assert!(nav.back());
        assert_eq!(nav.current(), &PageRequest::Home);
        assert!(nav.forward());
        assert_eq!(nav.current(), &album());

        // Opening a page from the middle of the history drops what was forward of it.
        nav.go(search());
        assert_eq!(nav.current(), &search());
        assert!(!nav.forward(), "Artist should be gone");
        assert!(nav.back());
        assert_eq!(nav.current(), &album());
        assert!(nav.back());
        assert_eq!(nav.current(), &PageRequest::Home);
    }

    #[test]
    fn going_to_the_current_page_adds_nothing() {
        let mut nav = Nav::default();
        nav.go(album());
        nav.go(album());
        assert!(nav.back());
        assert_eq!(nav.current(), &PageRequest::Home);
    }

    /// A search's words changing replace its page: Back leaves the search.
    #[test]
    fn replace_keeps_the_history() {
        let mut nav = Nav::default();
        nav.go(album());
        nav.go(PageRequest::Search {
            query: String::new(),
        });
        nav.replace(search());
        assert_eq!(nav.current(), &search());
        assert_eq!(nav.pages(), &[PageRequest::Home, album(), search()]);
        assert!(nav.back());
        assert_eq!(nav.current(), &album());
        assert!(nav.forward());
        assert_eq!(nav.current(), &search());
        assert!(!nav.forward());
    }

    #[test]
    fn history_keeps_the_latest_pages() {
        let mut nav = Nav::default();
        for i in 0..HISTORY_MAX + 10 {
            nav.go(PageRequest::Album {
                uri: format!("spotify:album:{i}"),
            });
        }
        let mut steps = 0;
        while nav.back() {
            steps += 1;
        }
        assert_eq!(steps, HISTORY_MAX - 1);
        // Home and albums 0 to 9 went: 111 pages in a history of 100.
        assert_eq!(
            nav.current(),
            &PageRequest::Album {
                uri: "spotify:album:10".into(),
            }
        );
    }

    #[test]
    fn page_names() {
        assert_eq!(page_name(&PageRequest::Home), "Home");
        assert_eq!(page_name(&search()), "Search");
        assert_eq!(page_name(&PageRequest::Recents), "Recently played");
        assert_eq!(page_name(&PageRequest::Liked), "Liked Songs");
        assert_eq!(page_name(&album()), "Album");
        assert_eq!(page_name(&artist()), "Artist");
    }

    /// Alt+← and Alt+→ move through the history; a bare arrow doesn't.
    #[test]
    fn alt_arrows_go_back_and_forward() {
        let mut nav = Nav::default();
        nav.go(album());
        let mut harness = Harness::new_ui_state(
            |ui, nav: &mut Nav| {
                nav.keys(ui.ctx());
            },
            nav,
        );
        harness.key_press_modifiers(Modifiers::ALT, Key::ArrowLeft);
        harness.run();
        assert_eq!(harness.state().current(), &PageRequest::Home);

        harness.key_press(Key::ArrowRight);
        harness.run();
        assert_eq!(harness.state().current(), &PageRequest::Home);

        harness.key_press_modifiers(Modifiers::ALT, Key::ArrowRight);
        harness.run();
        assert_eq!(harness.state().current(), &album());
    }
}
