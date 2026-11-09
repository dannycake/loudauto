//! The pathfinder (GraphQL) operations LOUDAUTO sends. The web player sends no query
//! text, only the sha256 of a query Spotify already knows (a persisted query), and
//! the hashes change when Spotify rebuilds the player. Each op carries the hash it
//! had when this table was written; [`super::registry::HashRegistry`] overrides it
//! with one scraped from the live web player.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Op {
    Home,
    SearchDesktop,
    ArtistOverview,
    GetAlbum,
    AlbumTracks,
    FetchPlaylist,
    LibraryTracks,
    Recents,
    InLibrary,
    AddToLibrary,
    RemoveFromLibrary,
}

impl Op {
    pub const ALL: [Op; 11] = [
        Op::Home,
        Op::SearchDesktop,
        Op::ArtistOverview,
        Op::GetAlbum,
        Op::AlbumTracks,
        Op::FetchPlaylist,
        Op::LibraryTracks,
        Op::Recents,
        Op::InLibrary,
        Op::AddToLibrary,
        Op::RemoveFromLibrary,
    ];

    /// The GraphQL `operationName`. It is also the key in the web player's bundle and
    /// in the hash cache.
    pub fn name(self) -> &'static str {
        match self {
            Op::Home => "home",
            Op::SearchDesktop => "searchDesktop",
            Op::ArtistOverview => "queryArtistOverview",
            Op::GetAlbum => "getAlbum",
            Op::AlbumTracks => "queryAlbumTracks",
            Op::FetchPlaylist => "fetchPlaylist",
            Op::LibraryTracks => "fetchLibraryTracks",
            Op::Recents => "recents",
            Op::InLibrary => "areEntitiesInLibrary",
            Op::AddToLibrary => "addToLibrary",
            Op::RemoveFromLibrary => "removeFromLibrary",
        }
    }

    /// The persisted-query hash as captured, used until a scrape finds a newer one.
    /// Two pairs share a hash in the capture: the album ops, and add/remove.
    pub fn fallback_hash(self) -> &'static str {
        match self {
            Op::Home => "5fb7da7a03ee2776c0670856609cb85a189a3cecc36cc160b91cde79063b9cf1",
            Op::SearchDesktop => "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03",
            Op::ArtistOverview => {
                "eafb1620d783ece118e72fbfede2b0064483e029d3c1aafafa1c2de0e25a89e7"
            }
            Op::GetAlbum => "0b6ce3511e1b02e4a40fa95440f992acbe85080eb308af9194322d708fc1914f",
            Op::AlbumTracks => "0b6ce3511e1b02e4a40fa95440f992acbe85080eb308af9194322d708fc1914f",
            Op::FetchPlaylist => "21ea11a6ac75a5f3e01fd365d29c18e0a4a8b7dc09f89bbb5d73910190c6acaf",
            Op::LibraryTracks => "087278b20b743578a6262c2b0b4bcd20d879c503cc359a2285baf083ef944240",
            Op::Recents => "94be830be40f0f3759ef83a4ea6e4dd1bef1f6108f7502c11ee1fa7fb1bb1658",
            Op::InLibrary => "134337999233cc6fdd6b1e6dbf94841409f04a946c5c7b744b09ba0dfe5a85ed",
            Op::AddToLibrary => "896ebcb47815681340860d121cb5d494e157e2a78d3950385cd54e0393c67148",
            Op::RemoveFromLibrary => {
                "896ebcb47815681340860d121cb5d494e157e2a78d3950385cd54e0393c67148"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn names_are_unique_and_hashes_64_hex() {
        let names: HashSet<&str> = Op::ALL.iter().map(|op| op.name()).collect();
        assert_eq!(names.len(), Op::ALL.len(), "duplicate op name");
        for op in Op::ALL {
            let hash = op.fallback_hash();
            assert_eq!(hash.len(), 64, "{op:?}");
            assert!(
                hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{op:?}: {hash}"
            );
        }
    }
}
