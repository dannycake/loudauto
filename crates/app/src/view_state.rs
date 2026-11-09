//! The window's view, kept across windows: the page and its history, scroll positions,
//! the right panel's tab and the window's size. A raise on Wayland is the old window
//! closing and a new one starting, so the new one restores this to look like the same
//! window coming forward. Saved on close to `$XDG_STATE_HOME/loudauto/window.json`
//! (else `~/.local/state/…`), loaded at start; a missing or unreadable file is the
//! defaults.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use engine_ipc::PageRequest;
use serde::{Deserialize, Serialize};

use crate::nav::Nav;

/// The window's size before one is saved, in points.
pub const DEFAULT_SIZE: [f32; 2] = [1200.0, 800.0];
/// The smallest the window may be, in points.
pub const MIN_SIZE: [f32; 2] = [900.0, 600.0];
/// The largest saved size taken as true, in points: past it the file is wrong.
const MAX_SIDE: f32 = 16384.0;

/// The right panel's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PanelTab {
    Queue,
    Lyrics,
    Credits,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewState {
    pub nav: Nav,
    /// Each page's scroll offset in points, by [`scroll_key`].
    pub scroll: HashMap<String, f32>,
    /// The right panel's tab; `None` while the panel is closed.
    pub panel: Option<PanelTab>,
    /// The window's inner size, in points.
    pub size: [f32; 2],
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            nav: Nav::default(),
            scroll: HashMap::new(),
            panel: None,
            size: DEFAULT_SIZE,
        }
    }
}

impl ViewState {
    /// `$XDG_STATE_HOME/loudauto/window.json`, else `~/.local/state/loudauto/window.json`;
    /// `None` with neither set.
    pub fn path() -> Option<PathBuf> {
        path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
    }

    /// The view saved at `path`; the defaults when there is none or it can't be read,
    /// and for each part of it that makes no sense.
    pub fn load(path: &Path) -> ViewState {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return ViewState::default(),
            Err(e) => {
                log::warn!("ignoring unreadable {}: {e}", path.display());
                return ViewState::default();
            }
        };
        match serde_json::from_slice::<ViewState>(&bytes) {
            Ok(view) => view.sanitized(),
            Err(e) => {
                log::warn!("ignoring {}: {e}", path.display());
                ViewState::default()
            }
        }
    }

    /// Writes the view to `path` (a temporary file renamed over it, so a window starting
    /// meanwhile reads the old view or the new, never half of one), with the scroll
    /// offsets of the pages in its history.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        // Only the offsets of pages the history can go back to: a search makes a page
        // per query, and the file would otherwise grow from one session to the next.
        let mut view = self.clone();
        let kept: HashSet<String> = view.nav.pages().iter().map(scroll_key).collect();
        view.scroll.retain(|key, _| kept.contains(key));
        let json = serde_json::to_vec_pretty(&view).map_err(io::Error::other)?;
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        let written = fs::write(&tmp, json).and_then(|()| fs::rename(&tmp, path));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written
    }

    /// The view with what makes no sense put back to its default: a history whose
    /// cursor is past its end, a size outside what a window can be, a scroll offset
    /// that isn't a distance.
    fn sanitized(mut self) -> ViewState {
        if !self.nav.is_valid() {
            self.nav = Nav::default();
        }
        let side = |v: f32, min: f32| v.is_finite() && (min..=MAX_SIDE).contains(&v);
        if !(side(self.size[0], MIN_SIZE[0]) && side(self.size[1], MIN_SIZE[1])) {
            self.size = DEFAULT_SIZE;
        }
        self.scroll
            .retain(|_, offset| offset.is_finite() && *offset >= 0.0);
        self
    }

    /// `req`'s saved scroll offset; 0 for one never scrolled.
    pub fn scroll_of(&self, req: &PageRequest) -> f32 {
        self.scroll.get(&scroll_key(req)).copied().unwrap_or(0.0)
    }

    pub fn set_scroll(&mut self, req: &PageRequest, offset: f32) {
        self.scroll.insert(scroll_key(req), offset);
    }
}

/// The key a page's scroll offset is saved under: the page request as JSON.
pub fn scroll_key(req: &PageRequest) -> String {
    serde_json::to_string(req).unwrap_or_default()
}

/// [`ViewState::path`] for given `XDG_STATE_HOME` and `HOME`. The XDG spec requires the
/// directory be absolute, so a relative or empty value counts as unset.
pub fn path_from(state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let dir = absolute(state_home).or_else(|| absolute(home).map(|h| h.join(".local/state")))?;
    Some(dir.join("loudauto/window.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewed() -> ViewState {
        let mut view = ViewState::default();
        view.nav.go(PageRequest::Album {
            uri: "spotify:album:a".into(),
        });
        view.nav.go(PageRequest::Liked);
        view.nav.back();
        view.set_scroll(&PageRequest::Liked, 1234.5);
        view.panel = Some(PanelTab::Lyrics);
        view.size = [1500.0, 950.0];
        view
    }

    #[test]
    fn view_state_roundtrip_and_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loudauto/window.json");

        // Nothing saved yet: the defaults, Home at 1200×800.
        assert_eq!(ViewState::load(&path), ViewState::default());
        assert_eq!(ViewState::default().nav.current(), &PageRequest::Home);
        assert_eq!(ViewState::default().size, [1200.0, 800.0]);

        let view = viewed();
        view.save(&path).unwrap();
        let back = ViewState::load(&path);
        assert_eq!(back, view);
        assert_eq!(
            back.nav.current(),
            &PageRequest::Album {
                uri: "spotify:album:a".into()
            }
        );
        assert_eq!(back.scroll_of(&PageRequest::Liked), 1234.5);
        assert_eq!(back.scroll_of(&PageRequest::Home), 0.0);
        // No temporary file left beside it.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("window.json")]);

        for garbage in [
            &b"not json"[..],
            b"",
            b"\xff\xfe",
            b"[1, 2, 3]",
            // A history with nothing in it, or a cursor past its end.
            br#"{"nav": {"history": [], "cursor": 0}}"#,
            br#"{"nav": {"history": [{"t": "Home"}], "cursor": 4}}"#,
            br#"{"nav": {"history": [{"t": "Nowhere"}], "cursor": 0}}"#,
        ] {
            std::fs::write(&path, garbage).unwrap();
            assert_eq!(
                ViewState::load(&path),
                ViewState::default(),
                "{}",
                String::from_utf8_lossy(garbage)
            );
        }
    }

    /// What is wrong in a saved view goes back to its default; the rest is kept.
    #[test]
    fn bad_fields_fall_back_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window.json");
        for size in ["[0, 0]", "[10, 10]", "[1e30, 800]", "[-1200, 800]"] {
            let json = format!(r#"{{"panel": "Queue", "size": {size}}}"#);
            std::fs::write(&path, json).unwrap();
            let view = ViewState::load(&path);
            assert_eq!(view.size, DEFAULT_SIZE, "{size}");
            assert_eq!(view.panel, Some(PanelTab::Queue));
        }
        std::fs::write(
            &path,
            r#"{"scroll": {"a": 10.0, "b": -5.0}, "size": [900, 600]}"#,
        )
        .unwrap();
        let view = ViewState::load(&path);
        assert_eq!(view.size, MIN_SIZE);
        assert_eq!(
            view.scroll,
            HashMap::from([("a".to_string(), 10.0)]),
            "a negative offset is dropped"
        );
    }

    /// Only the offsets of pages in the history are saved: a search makes a page per
    /// query, and the file would otherwise grow from one session to the next.
    #[test]
    fn saved_scroll_only_for_pages_in_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window.json");
        let mut view = viewed();
        let gone = PageRequest::Search { query: "j".into() };
        view.set_scroll(&gone, 50.0);
        view.set_scroll(&PageRequest::Home, 10.0);
        view.save(&path).unwrap();
        let back = ViewState::load(&path);
        assert_eq!(back.scroll_of(&gone), 0.0);
        assert_eq!(back.scroll_of(&PageRequest::Liked), 1234.5);
        assert_eq!(back.scroll_of(&PageRequest::Home), 10.0);
        assert_eq!(back.scroll.len(), 2);
    }

    #[test]
    fn state_path_honours_xdg_state_home() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            path_from(os("/x/state"), os("/home/u")),
            Some(PathBuf::from("/x/state/loudauto/window.json"))
        );
        for unset in [None, os(""), os("relative/state")] {
            assert_eq!(
                path_from(unset, os("/home/u")),
                Some(PathBuf::from("/home/u/.local/state/loudauto/window.json"))
            );
        }
        assert_eq!(path_from(None, None), None);
    }
}
