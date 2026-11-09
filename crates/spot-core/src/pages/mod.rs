//! What the window shows: pages parsed from the web player's pathfinder answers.
//! Every model crosses the engine-to-window socket, hence serde.

pub mod parse;

use serde::{Deserialize, Serialize};

pub use parse::{
    parse_album, parse_album_tracks, parse_artist, parse_credits, parse_home, parse_in_library,
    parse_liked, parse_lyrics, parse_playlist, parse_radio, parse_recents, parse_search,
};

/// One size of a cover or avatar. `width` is `None` when Spotify doesn't say.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Image {
    pub url: String,
    pub width: Option<u32>,
}

/// The image to draw at `px` pixels wide: the smallest at least that wide, else the
/// largest. An unknown width counts as larger than any known one.
pub fn pick_image(images: &[Image], px: u32) -> Option<&Image> {
    let width = |i: &&Image| i.width.map_or(u64::MAX, u64::from);
    images
        .iter()
        .filter(|i| width(i) >= u64::from(px))
        .min_by_key(width)
        .or_else(|| images.iter().max_by_key(width))
}

/// Something with a URI and a name: an artist or an album on a track row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Named {
    pub uri: String,
    pub name: String,
}

/// One row of a track list. `uid` tells two copies of a track in one playlist apart;
/// `images` are the row's own cover (empty on album rows, which share the album's).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRow {
    pub uri: String,
    pub uid: Option<String>,
    pub title: String,
    pub artists: Vec<Named>,
    pub album: Option<Named>,
    pub duration_ms: u64,
    pub images: Vec<Image>,
    /// When the track was added to the playlist or library, ISO 8601 as Spotify gives it.
    pub added_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlbumPage {
    pub uri: String,
    pub name: String,
    pub artists: Vec<Named>,
    /// `YYYY-MM-DD`, `YYYY-MM` or `YYYY`, as precise as Spotify knows it.
    pub release_date: String,
    pub label: String,
    pub images: Vec<Image>,
    pub total: u32,
    pub tracks: Vec<TrackRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistPage {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub images: Vec<Image>,
    pub total: u32,
    pub tracks: Vec<TrackRow>,
}

/// One page of Liked Songs; `total` is the whole library's count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LikedPage {
    pub total: u32,
    pub tracks: Vec<TrackRow>,
}

/// What a card opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardKind {
    Album,
    Playlist,
    Artist,
    Track,
    /// A kind this client has no page for.
    Other,
}

/// A tile on Home, in search results, on an artist page or in Recently played.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub uri: String,
    pub kind: CardKind,
    pub title: String,
    /// The line under the title: artists, a playlist's description, "Artist" or a year.
    pub subtitle: String,
    pub images: Vec<Image>,
}

/// Spotify's Home, Music facet only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Home {
    pub greeting: String,
    pub sections: Vec<Section>,
}

/// One titled row of cards on Home; never empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub title: String,
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResults {
    pub tracks: Vec<TrackRow>,
    pub artists: Vec<Card>,
    pub albums: Vec<Card>,
    pub playlists: Vec<Card>,
}

/// An artist's overview. `header` is the wide banner, `avatar` the round portrait;
/// either can be empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtistPage {
    pub uri: String,
    pub name: String,
    pub header: Vec<Image>,
    pub avatar: Vec<Image>,
    pub top_tracks: Vec<TrackRow>,
    pub albums: Vec<Card>,
    pub singles: Vec<Card>,
    pub related: Vec<Card>,
}

/// A track's lyrics. When `synced` is false the lines carry no usable times.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lyrics {
    pub synced: bool,
    pub provider: String,
    pub lines: Vec<LyricLine>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricLine {
    pub start_ms: u64,
    pub words: String,
}

/// One group of a track's credits, such as "Writers", and who is in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreditRole {
    pub role: String,
    pub names: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(width: Option<u32>) -> Image {
        Image {
            url: format!("u{width:?}"),
            width,
        }
    }

    #[test]
    fn pick_image_cases() {
        let set = [img(Some(300)), img(Some(64)), img(Some(640))];
        assert_eq!(pick_image(&set, 40), Some(&set[1]));
        assert_eq!(pick_image(&set, 300), Some(&set[0]));
        assert_eq!(pick_image(&set, 1000), Some(&set[2]));

        let unknown = [img(None), img(Some(64))];
        assert_eq!(pick_image(&unknown, 100), Some(&unknown[0]));

        assert_eq!(pick_image(&[], 100), None);
    }
}
