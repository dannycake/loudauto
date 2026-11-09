//! Plain data types; nothing here depends on librespot.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistRef {
    pub uri: String,
    pub name: String,
}

/// One row of the user's playlist sidebar (the "rootlist"). It crosses the
/// engine-to-window socket, hence serde.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RootEntry {
    Playlist(PlaylistRef),
    FolderStart { id: String, name: String },
    FolderEnd { id: String },
    Other { uri: String },
}

/// An item in Liked Songs or a search result.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryEntry {
    pub uri: String,
    /// Unix milliseconds, when Spotify supplies it (Liked Songs).
    pub added_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Track,
    Episode,
    Local,
    Other,
}

pub fn kind_of(uri: &str) -> ItemKind {
    if uri.starts_with("spotify:track:") {
        ItemKind::Track
    } else if uri.starts_with("spotify:episode:") {
        ItemKind::Episode
    } else if uri.starts_with("spotify:local:") {
        ItemKind::Local
    } else {
        ItemKind::Other
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub uri: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: u32,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistDetail {
    pub uri: String,
    pub name: String,
    pub items: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_of_classifies_by_prefix() {
        assert_eq!(kind_of("spotify:track:x"), ItemKind::Track);
        assert_eq!(kind_of("spotify:episode:x"), ItemKind::Episode);
        assert_eq!(kind_of("spotify:local:a:b:c:180"), ItemKind::Local);
        assert_eq!(kind_of("spotify:show:x"), ItemKind::Other);
    }

    #[test]
    fn root_entries_roundtrip_through_json() {
        let entries = vec![
            RootEntry::Playlist(PlaylistRef {
                uri: "spotify:playlist:p".into(),
                name: "Main".into(),
            }),
            RootEntry::FolderStart {
                id: "f".into(),
                name: "Folder".into(),
            },
            RootEntry::FolderEnd { id: "f".into() },
            RootEntry::Other {
                uri: "spotify:x".into(),
            },
        ];
        let json = serde_json::to_string(&entries).unwrap();
        let back: Vec<RootEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entries);
    }
}
