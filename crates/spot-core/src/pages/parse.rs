//! Pages from pathfinder bodies (album, playlist, Liked Songs, Home, search, artist,
//! Recently played, heart state) and spclient bodies (lyrics, credits, radio).
//!
//! Each parser takes the whole JSON body. A missing optional field takes its default;
//! a missing page object is [`SpotError::UnexpectedPage`]; a row that isn't a playable
//! Spotify track (removed, unavailable, a local file, an episode) is skipped, and the
//! skips are logged once per page. A card without a URI or a title is left out.

use serde_json::Value;

use super::{
    AlbumPage, ArtistPage, Card, CardKind, CreditRole, Home, Image, LikedPage, LyricLine, Lyrics,
    Named, PlaylistPage, SearchResults, Section, TrackRow,
};
use crate::{Result, SpotError};

/// `getAlbum`: the album and its first page of tracks. Rows carry the album as `album`
/// and no images of their own.
pub fn parse_album(body: &Value) -> Result<AlbumPage> {
    let album = page_object(body, "/data/albumUnion", "getAlbum")?;
    let uri = string(album, "/uri");
    let name = string(album, "/name");
    let named = Named {
        uri: uri.clone(),
        name: name.clone(),
    };
    let tracks = album_rows(album, Some(&named), &uri);
    Ok(AlbumPage {
        artists: artists(album.pointer("/artists/items")),
        release_date: release_date(album.pointer("/date")),
        label: string(album, "/label"),
        images: images(album.pointer("/coverArt/sources")),
        total: count(album, "/tracksV2/totalCount"),
        uri,
        name,
        tracks,
    })
}

/// `queryAlbumTracks`: a later page of an album's tracks. The body doesn't name the
/// album, so the rows' `album` is `None`; the caller knows it.
pub fn parse_album_tracks(body: &Value) -> Result<Vec<TrackRow>> {
    let album = page_object(body, "/data/albumUnion", "queryAlbumTracks")?;
    Ok(album_rows(album, None, "album tracks"))
}

/// `fetchPlaylist`: the playlist and one page of its rows.
pub fn parse_playlist(body: &Value) -> Result<PlaylistPage> {
    let list = page_object(body, "/data/playlistV2", "fetchPlaylist")?;
    let uri = string(list, "/uri");
    let tracks = rows(list.pointer("/content/items"), &uri, |item| {
        let data = item.pointer("/itemV2/data")?;
        track_row(
            data,
            data.get("uri"),
            "/trackDuration/totalMilliseconds",
            item,
        )
    });
    Ok(PlaylistPage {
        name: string(list, "/name"),
        description: string(list, "/description"),
        images: images(list.pointer("/images/items/0/sources")),
        total: count(list, "/content/totalCount"),
        uri,
        tracks,
    })
}

/// `fetchLibraryTracks`: one page of Liked Songs. Library rows have no uid.
pub fn parse_liked(body: &Value) -> Result<LikedPage> {
    let library = page_object(body, "/data/me/library/tracks", "fetchLibraryTracks")?;
    let tracks = rows(library.get("items"), "liked songs", |item| {
        let track = item.get("track")?;
        track_row(
            track.get("data")?,
            track.get("_uri"),
            "/duration/totalMilliseconds",
            item,
        )
    });
    Ok(LikedPage {
        total: count(library, "/totalCount"),
        tracks,
    })
}

/// `home`: the greeting and Spotify's sections, Music facet only. A card is an album,
/// playlist or artist; anything else (episodes, podcasts, audiobooks) is left out, and
/// so is a section without a title or with no cards left.
pub fn parse_home(body: &Value) -> Result<Home> {
    let home = page_object(body, "/data/home", "home")?;
    let sections = array(home.pointer("/sectionContainer/sections/items"))
        .iter()
        .filter_map(|section| {
            let title = section
                .pointer("/data/title/transformedLabel")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())?;
            let cards = cards(section.pointer("/sectionItems/items"), |item| {
                music_card(item.pointer("/content/data")?)
            });
            (!cards.is_empty()).then(|| Section {
                title: title.to_string(),
                cards,
            })
        })
        .collect();
    Ok(Home {
        greeting: string(home, "/greeting/transformedLabel"),
        sections,
    })
}

/// `searchDesktop`: the first page of tracks, artists, albums and playlists.
pub fn parse_search(body: &Value) -> Result<SearchResults> {
    let search = page_object(body, "/data/searchV2", "searchDesktop")?;
    let tracks = rows(search.pointer("/tracksV2/items"), "search", |item| {
        let data = item.pointer("/item/data")?;
        track_row(data, data.get("uri"), "/duration/totalMilliseconds", item)
    });
    let of = |pointer: &str, card: fn(&Value) -> Option<Card>| {
        cards(search.pointer(pointer), |item| card(item.get("data")?))
    };
    Ok(SearchResults {
        tracks,
        artists: of("/artists/items", artist_card),
        albums: of("/albumsV2/items", album_card),
        playlists: of("/playlists/items", playlist_card),
    })
}

/// `queryArtistOverview`: the artist, their top tracks (with uids), the first release
/// (`releases.items[0]`) of each album and single group, and related artists.
pub fn parse_artist(body: &Value) -> Result<ArtistPage> {
    let artist = page_object(body, "/data/artistUnion", "queryArtistOverview")?;
    let uri = string(artist, "/uri");
    let top_tracks = rows(
        artist.pointer("/discography/topTracks/items"),
        &uri,
        |item| {
            let track = item.get("track")?;
            track_row(track, track.get("uri"), "/duration/totalMilliseconds", item)
        },
    );
    let releases = |group: &str| {
        cards(
            artist.pointer(&format!("/discography/{group}/items")),
            |item| release_card(item.pointer("/releases/items/0")?),
        )
    };
    Ok(ArtistPage {
        name: string(artist, "/profile/name"),
        header: images(artist.pointer("/headerImage/data/sources")),
        avatar: images(artist.pointer("/visuals/avatarImage/sources")),
        top_tracks,
        albums: releases("albums"),
        singles: releases("singles"),
        related: cards(
            artist.pointer("/relatedContent/relatedArtists/items"),
            artist_card,
        ),
        uri,
    })
}

/// `recents`: Recently played in Spotify's order, each under its contributors' names. An
/// entity type this client has no page for comes back as [`CardKind::Other`].
pub fn parse_recents(body: &Value) -> Result<Vec<Card>> {
    let list = page_object(body, "/data/lists/0", "recents")?;
    Ok(cards(list.pointer("/items/items"), |item| {
        let entity = item.get("entity")?;
        let data = entity.get("data")?;
        let kind = match data
            .pointer("/entityTypeTrait/type")
            .and_then(Value::as_str)
        {
            Some("ENTITY_TYPE_TRACK") => CardKind::Track,
            Some("ENTITY_TYPE_ALBUM") => CardKind::Album,
            Some("ENTITY_TYPE_PLAYLIST") => CardKind::Playlist,
            Some("ENTITY_TYPE_ARTIST") => CardKind::Artist,
            _ => CardKind::Other,
        };
        card(
            kind,
            entity.get("_uri"),
            data.pointer("/identityTrait/name"),
            names(data.pointer("/identityTrait/contributors/items"), "/name").join(", "),
            images(data.pointer("/visualIdentityTrait/squareCoverImage/image/data/sources")),
        )
    }))
}

/// `areEntitiesInLibrary`: one answer per asked URI, in the same order. An entry without
/// a `saved` flag (a URI Spotify doesn't know) counts as not saved.
pub fn parse_in_library(body: &Value) -> Result<Vec<bool>> {
    let lookup = body
        .pointer("/data/lookup")
        .and_then(Value::as_array)
        .ok_or_else(|| missing("areEntitiesInLibrary", "/data/lookup"))?;
    Ok(lookup
        .iter()
        .map(|entry| {
            entry
                .pointer("/data/saved")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .collect())
}

/// `color-lyrics`: the lines, and whether they are line-synced. `startTimeMs` comes as
/// a string; a line without words is left out.
pub fn parse_lyrics(body: &Value) -> Result<Lyrics> {
    let lyrics = body
        .get("lyrics")
        .filter(|v| v.is_object())
        .ok_or_else(|| missing("color-lyrics", "/lyrics"))?;
    let lines = array(lyrics.get("lines"))
        .iter()
        .filter_map(|line| {
            Some(LyricLine {
                start_ms: millis(line.get("startTimeMs")),
                words: line.get("words")?.as_str()?.to_string(),
            })
        })
        .collect();
    Ok(Lyrics {
        synced: lyrics.get("syncType").and_then(Value::as_str) == Some("LINE_SYNCED"),
        provider: string(lyrics, "/provider"),
        lines,
    })
}

/// `track-credits-view`: each role and the names in it; a role with no names is left
/// out.
pub fn parse_credits(body: &Value) -> Result<Vec<CreditRole>> {
    let roles = body
        .get("roleCredits")
        .and_then(Value::as_array)
        .ok_or_else(|| missing("track credits", "/roleCredits"))?;
    Ok(roles
        .iter()
        .filter_map(|role| {
            let names = names(role.get("artists"), "/name");
            (!names.is_empty()).then(|| CreditRole {
                role: string(role, "/roleTitle"),
                names,
            })
        })
        .collect())
}

/// `inspiredby-mix` seed to playlist: the radio playlist's URI, or `None` when Spotify
/// made none for the seed.
pub fn parse_radio(body: &Value) -> Option<String> {
    opt_string(body, "/mediaItems/0/uri").filter(|uri| !uri.is_empty())
}

/// The object at `pointer`, or `UnexpectedPage` naming the op when it is absent, null
/// or Spotify's `NotFound` stand-in.
fn page_object<'a>(body: &'a Value, pointer: &str, op: &str) -> Result<&'a Value> {
    match body.pointer(pointer) {
        Some(v) if v.is_object() && v.get("__typename") != Some(&Value::from("NotFound")) => Ok(v),
        _ => Err(missing(op, pointer)),
    }
}

/// `UnexpectedPage` for a body from `op` that lacks `pointer`.
fn missing(op: &str, pointer: &str) -> SpotError {
    SpotError::UnexpectedPage(format!(
        "{op} (no {})",
        pointer.trim_start_matches('/').replace('/', ".")
    ))
}

/// Album rows (`tracksV2.items[].{uid, track}`), which share the album's cover.
fn album_rows(album: &Value, named: Option<&Named>, what: &str) -> Vec<TrackRow> {
    rows(album.pointer("/tracksV2/items"), what, |item| {
        let track = item.get("track")?;
        let mut row = track_row(track, track.get("uri"), "/duration/totalMilliseconds", item)?;
        row.album = named.cloned();
        row.images.clear();
        Some(row)
    })
}

/// Parse each item of a row array with `parse`, skipping (and counting) the ones it
/// rejects; one warning per page names the count.
fn rows(
    items: Option<&Value>,
    what: &str,
    parse: impl Fn(&Value) -> Option<TrackRow>,
) -> Vec<TrackRow> {
    let items = array(items);
    let tracks: Vec<TrackRow> = items.iter().filter_map(parse).collect();
    let skipped = items.len() - tracks.len();
    if skipped > 0 {
        log::warn!(
            "{what}: skipped {skipped} of {} rows that aren't playable tracks",
            items.len()
        );
    }
    tracks
}

/// A row from a track object, `None` unless it has a name and a `spotify:track:` URI
/// (which leaves out removed rows, local files and episodes). `item` is the row
/// wrapper holding `uid` and `addedAt`.
fn track_row(track: &Value, uri: Option<&Value>, duration: &str, item: &Value) -> Option<TrackRow> {
    let uri = uri?.as_str().filter(|u| u.starts_with("spotify:track:"))?;
    let title = track.get("name")?.as_str()?;
    let album = track.get("albumOfTrack");
    Some(TrackRow {
        uri: uri.to_string(),
        uid: opt_string(item, "/uid"),
        title: title.to_string(),
        artists: artists(track.pointer("/artists/items")),
        album: album.and_then(named),
        duration_ms: track.pointer(duration).and_then(Value::as_u64).unwrap_or(0),
        images: images(album.and_then(|a| a.pointer("/coverArt/sources"))),
        added_at: opt_string(item, "/addedAt/isoString"),
    })
}

/// Cards from each item of an array, leaving out the ones `card` rejects.
fn cards(items: Option<&Value>, card: impl Fn(&Value) -> Option<Card>) -> Vec<Card> {
    array(items).iter().filter_map(card).collect()
}

/// A card, `None` without a URI or a title.
fn card(
    kind: CardKind,
    uri: Option<&Value>,
    title: Option<&Value>,
    subtitle: String,
    images: Vec<Image>,
) -> Option<Card> {
    Some(Card {
        uri: uri?.as_str().filter(|u| !u.is_empty())?.to_string(),
        kind,
        title: title?.as_str()?.to_string(),
        subtitle,
        images,
    })
}

/// A Home card by `__typename`: albums, playlists and artists only.
fn music_card(data: &Value) -> Option<Card> {
    match data.get("__typename")?.as_str()? {
        "Album" => album_card(data),
        "Playlist" => playlist_card(data),
        "Artist" => artist_card(data),
        _ => None,
    }
}

/// An album under its artists' names.
fn album_card(album: &Value) -> Option<Card> {
    card(
        CardKind::Album,
        album.get("uri"),
        album.get("name"),
        names(album.pointer("/artists/items"), "/profile/name").join(", "),
        images(album.pointer("/coverArt/sources")),
    )
}

/// Liked Songs as Recently played names it.
const LIKED_SONGS_URI: &str = "spotify:collection:tracks";

/// A playlist under its description. Home's Liked Songs tile is a placeholder playlist
/// (`format: "liked-songs"`, no items), so it gets the Liked Songs URI instead.
fn playlist_card(list: &Value) -> Option<Card> {
    let mut card = card(
        CardKind::Playlist,
        list.get("uri"),
        list.get("name"),
        string(list, "/description"),
        images(list.pointer("/images/items/0/sources")),
    )?;
    if list.get("format").and_then(Value::as_str) == Some("liked-songs") {
        card.uri = LIKED_SONGS_URI.to_string();
    }
    Some(card)
}

fn artist_card(artist: &Value) -> Option<Card> {
    card(
        CardKind::Artist,
        artist.get("uri"),
        artist.pointer("/profile/name"),
        "Artist".to_string(),
        images(artist.pointer("/visuals/avatarImage/sources")),
    )
}

/// A release on an artist page, under its year. Releases carry no artists: the page is
/// the artist.
fn release_card(release: &Value) -> Option<Card> {
    let year = release
        .pointer("/date/year")
        .and_then(Value::as_u64)
        .map(|y| y.to_string())
        .unwrap_or_default();
    card(
        CardKind::Album,
        release.get("uri"),
        release.get("name"),
        year,
        images(release.pointer("/coverArt/sources")),
    )
}

/// The string at `pointer` in each item of an array; items without one are left out.
fn names(items: Option<&Value>, pointer: &str) -> Vec<String> {
    array(items)
        .iter()
        .filter_map(|item| opt_string(item, pointer))
        .collect()
}

/// Artists from `[{uri, profile.name}]`; an entry missing either is left out.
fn artists(items: Option<&Value>) -> Vec<Named> {
    let items = array(items);
    items
        .iter()
        .filter_map(|a| {
            Some(Named {
                uri: a.get("uri")?.as_str()?.to_string(),
                name: a.pointer("/profile/name")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// `{uri, name}` when both are there.
fn named(v: &Value) -> Option<Named> {
    Some(Named {
        uri: v.get("uri")?.as_str()?.to_string(),
        name: v.get("name")?.as_str()?.to_string(),
    })
}

/// Images from `[{url, width}]` or `[{url, maxWidth}]`; a source without a URL is left
/// out, and a width of 0 counts as unknown.
fn images(sources: Option<&Value>) -> Vec<Image> {
    let sources = array(sources);
    sources
        .iter()
        .filter_map(|s| {
            let width = |key| s.get(key).and_then(Value::as_u64);
            Some(Image {
                url: s.get("url")?.as_str()?.to_string(),
                width: width("width")
                    .or_else(|| width("maxWidth"))
                    .filter(|&w| w > 0)
                    .and_then(|w| u32::try_from(w).ok()),
            })
        })
        .collect()
}

/// `date.isoString` cut to `date.precision`: `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
fn release_date(date: Option<&Value>) -> String {
    let Some(date) = date else {
        return String::new();
    };
    let iso = date
        .get("isoString")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let day = iso.split('T').next().unwrap_or_default();
    let len = match date.get("precision").and_then(Value::as_str) {
        Some("YEAR") => 4,
        Some("MONTH") => 7,
        _ => day.len(),
    };
    day.get(..len).unwrap_or(day).to_string()
}

/// The array at `v`, or nothing when it is absent or not an array.
fn array(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn opt_string(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn string(v: &Value, pointer: &str) -> String {
    opt_string(v, pointer).unwrap_or_default()
}

fn count(v: &Value, pointer: &str) -> u32 {
    v.pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// Milliseconds given as a string (`"2850"`) or a number; 0 when absent or unreadable.
fn millis(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(v) => v.as_u64().unwrap_or(0),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::pages::Named;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/web/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    fn jane() -> Named {
        Named {
            uri: "spotify:artist:2rLGlNI6htigNxx172qxLu".into(),
            name: "Jane Remover".into(),
        }
    }

    #[test]
    fn album_page() {
        let page = parse_album(&fixture("album.json")).unwrap();
        assert_eq!(page.uri, "spotify:album:21b4cDNse2AMpj94ykfuON");
        assert_eq!(page.name, "Revengeseekerz");
        assert_eq!(page.label, "deadAir");
        assert_eq!(page.release_date, "2025-04-04");
        assert_eq!(page.artists, vec![jane()]);
        assert_eq!(page.images.len(), 3);
        assert_eq!(page.total, 12);
        assert_eq!(page.tracks.len(), 12);

        let row = &page.tracks[0];
        assert_eq!(row.title, "TWICE REMOVED");
        assert_eq!(row.uid.as_deref(), Some("15c2b07abdb6df778781"));
        assert_eq!(row.uri, "spotify:track:6cMaeMxUUtPGZri4qPSQ5Y");
        assert_eq!(row.duration_ms, 239161);
        assert_eq!(row.artists, vec![jane()]);
        assert_eq!(
            row.album,
            Some(Named {
                uri: page.uri.clone(),
                name: "Revengeseekerz".into()
            })
        );
        assert!(row.images.is_empty());
        assert_eq!(row.added_at, None);
        assert_eq!(page.tracks[7].title, "Dancing with your eyes closed");
    }

    #[test]
    fn album_tracks_page() {
        let rows = parse_album_tracks(&fixture("album_tracks.json")).unwrap();
        assert_eq!(rows.len(), 12);
        assert_eq!(rows[0].uid.as_deref(), Some("15c2b07abdb6df778781"));
        assert_eq!(rows[0].title, "TWICE REMOVED");
        assert_eq!(rows[0].duration_ms, 239161);
        assert_eq!(rows[0].artists, vec![jane()]);
        assert_eq!(rows[0].album, None);
    }

    #[test]
    fn release_date_follows_precision() {
        let mut body = fixture("album.json");
        let date = body.pointer_mut("/data/albumUnion/date").unwrap();
        *date = json!({"isoString": "2003-01-01T00:00:00Z", "precision": "YEAR"});
        assert_eq!(parse_album(&body).unwrap().release_date, "2003");
        *body.pointer_mut("/data/albumUnion/date").unwrap() =
            json!({"isoString": "2003-05-01T00:00:00Z", "precision": "MONTH"});
        assert_eq!(parse_album(&body).unwrap().release_date, "2003-05");
    }

    #[test]
    fn playlist_page() {
        let page = parse_playlist(&fixture("playlist.json")).unwrap();
        assert_eq!(page.uri, "spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi");
        assert_eq!(page.name, "Main");
        assert_eq!(page.description, "");
        assert_eq!(page.total, 81);
        assert!(!page.images.is_empty());
        assert_eq!(page.tracks.len(), 3);

        let row = &page.tracks[0];
        assert_eq!(row.title, "Interview");
        assert_eq!(row.uri, "spotify:track:4SWyOAtHVNdnczEIsLvMN2");
        assert_eq!(row.uid.as_deref(), Some("8a2f49e468f24758"));
        assert_eq!(row.duration_ms, 90947);
        assert_eq!(row.added_at.as_deref(), Some("2025-07-29T15:40:21Z"));
        assert_eq!(
            row.artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["SINN6R"]
        );
        assert_eq!(row.album.as_ref().map(|a| a.name.as_str()), Some("2Often"));
        assert!(!row.images.is_empty());
    }

    fn playlist_items(body: &mut Value) -> &mut Vec<Value> {
        body.pointer_mut("/data/playlistV2/content/items")
            .and_then(Value::as_array_mut)
            .unwrap()
    }

    #[test]
    fn playlist_skips_bad_rows() {
        let mut body = fixture("playlist.json");
        let items = playlist_items(&mut body);
        items[1]["itemV2"]["data"] = json!({"__typename": "NotFound"});
        let mut copy = items[2].clone();
        copy["uid"] = json!("0000000000000002");
        items.push(copy);

        let page = parse_playlist(&body).unwrap();
        assert_eq!(page.tracks.len(), 3);
        assert_eq!(page.tracks[0].title, "Interview");
        let (a, b) = (&page.tracks[1], &page.tracks[2]);
        assert_eq!(a.uri, "spotify:track:0DoZMCpsZaZHVw1eLCdTBM");
        assert_eq!(a.uri, b.uri);
        assert_ne!(a.uid, b.uid);
    }

    #[test]
    fn playlist_skips_null_local_and_episode_rows() {
        let mut body = fixture("playlist.json");
        let items = playlist_items(&mut body);
        let good = items[0].clone();
        let mut null_item = good.clone();
        null_item["itemV2"] = Value::Null;
        let mut null_data = good.clone();
        null_data["itemV2"]["data"] = Value::Null;
        let mut local = good.clone();
        local["itemV2"]["data"]["__typename"] = json!("LocalTrack");
        local["itemV2"]["data"]["uri"] = json!("spotify:local:SINN6R:2Often:Interview:90");
        let mut episode = good.clone();
        episode["itemV2"]["data"] = json!({
            "__typename": "Episode",
            "uri": "spotify:episode:0000000000000000000000",
            "name": "An episode",
        });
        *items = vec![null_item, null_data, local, episode, good];

        let page = parse_playlist(&body).unwrap();
        assert_eq!(page.tracks.len(), 1);
        assert_eq!(page.tracks[0].title, "Interview");
    }

    #[test]
    fn liked_page() {
        let page = parse_liked(&fixture("liked_tracks.json")).unwrap();
        assert_eq!(page.total, 758);
        assert_eq!(page.tracks.len(), 3);

        let row = &page.tracks[0];
        assert_eq!(row.uri, "spotify:track:1XD4K4CGAKTIBmFpvuaFru");
        assert_eq!(row.uid, None);
        assert_eq!(row.title, "Dancing with your eyes closed");
        assert_eq!(row.added_at.as_deref(), Some("2026-11-03T04:51:13Z"));
        assert_eq!(row.duration_ms, 230526);
        assert_eq!(row.artists, vec![jane()]);
        assert_eq!(
            row.album,
            Some(Named {
                uri: "spotify:album:21b4cDNse2AMpj94ykfuON".into(),
                name: "Revengeseekerz".into(),
            })
        );
        assert_eq!(row.images.len(), 3);
    }

    #[test]
    fn missing_top_level_is_unexpected_page() {
        let empty = json!({"data": {}});
        let not_found = json!({"data": {"albumUnion": {"__typename": "NotFound"}}});
        let unexpected = |r: Result<()>| matches!(r, Err(SpotError::UnexpectedPage(_)));
        assert!(unexpected(parse_album(&empty).map(drop)));
        assert!(unexpected(parse_album(&not_found).map(drop)));
        assert!(unexpected(parse_album_tracks(&empty).map(drop)));
        assert!(unexpected(parse_playlist(&empty).map(drop)));
        assert!(unexpected(
            parse_playlist(&json!({"data": {"playlistV2": null}})).map(drop)
        ));
        assert!(unexpected(parse_liked(&empty).map(drop)));
    }

    fn titles(cards: &[Card]) -> Vec<&str> {
        cards.iter().map(|c| c.title.as_str()).collect()
    }

    fn sections(body: &mut Value) -> &mut Vec<Value> {
        body.pointer_mut("/data/home/sectionContainer/sections/items")
            .and_then(Value::as_array_mut)
            .unwrap()
    }

    #[test]
    fn home() {
        let home = parse_home(&fixture("home.json")).unwrap();
        assert_eq!(home.greeting, "Good evening");
        let section_titles: Vec<&str> = home.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            section_titles,
            [
                "Jump back in",
                "Recently played",
                "Made For dannycake",
                "More like Lucy Bedroque"
            ]
        );

        let first = &home.sections[0].cards[0];
        assert_eq!(first.title, "PRETTY SURGEON");
        assert_eq!(first.uri, "spotify:album:6jV98kIvbHjhRiIeakALqF");
        assert_eq!(first.kind, CardKind::Album);
        assert_eq!(first.subtitle, "ZØMB");
        assert_eq!(first.images.len(), 3);

        let recent = &home.sections[1].cards;
        assert_eq!(recent[0].title, "Liked Songs");
        assert_eq!(recent[0].uri, "spotify:collection:tracks");
        assert_eq!(recent[0].kind, CardKind::Playlist);
        assert_eq!(recent[0].images.len(), 1);
        assert_eq!(recent[0].images[0].width, None);
        assert_eq!(
            recent[1].subtitle,
            "curated from Main + Last.fm history, 2026-08"
        );
        assert_eq!(recent[1].uri, "spotify:playlist:21erFbLzdJurEYKz86wDwv");
        assert_eq!(recent[2].kind, CardKind::Album);

        let more = &home.sections[3].cards[0];
        assert_eq!(more.title, "lostrushi");
        assert_eq!(more.uri, "spotify:artist:5Q2SPodgRF5DgI5Vs8y0Ve");
        assert_eq!(more.kind, CardKind::Artist);
        assert_eq!(more.subtitle, "Artist");
        assert_eq!(more.images.len(), 3);
    }

    #[test]
    fn home_keeps_music_cards_and_titled_sections() {
        let mut body = fixture("home.json");
        let secs = sections(&mut body);
        let episode = secs[3]["sectionItems"]["items"][0].clone();
        secs[4]["sectionItems"]["items"]
            .as_array_mut()
            .unwrap()
            .insert(1, episode);
        let mut podcast = secs[4]["sectionItems"]["items"][0].clone();
        podcast["content"]["data"] = json!({
            "__typename": "Podcast",
            "uri": "spotify:show:0000000000000000000000",
            "name": "A show",
        });
        secs[4]["sectionItems"]["items"]
            .as_array_mut()
            .unwrap()
            .push(podcast);
        secs[0]["data"]["title"]["transformedLabel"] = json!("");
        secs[2]["data"]["title"] = Value::Null;

        let home = parse_home(&body).unwrap();
        let section_titles: Vec<&str> = home.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            section_titles,
            ["Recently played", "More like Lucy Bedroque"]
        );
        assert_eq!(
            titles(&home.sections[1].cards),
            ["lostrushi", "medulla", "Egobreak"]
        );
    }

    #[test]
    fn search() {
        let results = parse_search(&fixture("search.json")).unwrap();
        assert_eq!(results.tracks.len(), 3);
        let row = &results.tracks[0];
        assert_eq!(row.title, "How to Teleport");
        assert_eq!(row.uri, "spotify:track:5BUTPG0Csnl1fF5RFg5lUH");
        assert_eq!(row.duration_ms, 273285);
        assert_eq!(row.album.as_ref().map(|a| a.name.as_str()), Some("♡"));
        assert_eq!(row.artists, vec![jane()]);
        assert_eq!(row.images.len(), 3);

        assert_eq!(results.artists[0].title, "Jane Remover");
        assert_eq!(results.artists[0].kind, CardKind::Artist);
        assert_eq!(results.artists[0].uri, jane().uri);
        assert!(!results.artists[0].images.is_empty());
        assert_eq!(results.albums[0].title, "Revengeseekerz");
        assert_eq!(results.albums[0].kind, CardKind::Album);
        assert_eq!(results.albums[0].subtitle, "Jane Remover");
        assert_eq!(results.playlists.len(), 3);
        assert!(
            results
                .playlists
                .iter()
                .all(|p| p.kind == CardKind::Playlist && p.uri.starts_with("spotify:playlist:"))
        );
    }

    #[test]
    fn artist() {
        let page = parse_artist(&fixture("artist.json")).unwrap();
        assert_eq!(page.uri, jane().uri);
        assert_eq!(page.name, "Jane Remover");
        assert_eq!(page.header.len(), 5);
        assert_eq!(page.header[0].width, Some(1494));
        assert_eq!(page.avatar.len(), 3);

        let top = &page.top_tracks[0];
        assert_eq!(top.title, "Beauty Sleep");
        assert_eq!(top.duration_ms, 222967);
        assert_eq!(top.uid.as_deref(), Some("abd2dc01d742f486c10f"));
        assert_eq!(top.artists, vec![jane()]);
        assert_eq!(page.top_tracks.len(), 3);

        assert_eq!(
            titles(&page.albums),
            ["Revengeseekerz", "Census Designated", "Frailty"]
        );
        assert_eq!(page.albums[0].uri, "spotify:album:21b4cDNse2AMpj94ykfuON");
        assert_eq!(page.albums[0].kind, CardKind::Album);
        assert_eq!(page.albums[0].subtitle, "2025");
        assert_eq!(page.albums[0].images.len(), 3);
        assert_eq!(page.singles[0].title, "Beauty Sleep");
        assert_eq!(
            titles(&page.related),
            ["underscores", "kmoe", "Frost Children"]
        );
        assert_eq!(page.related[0].kind, CardKind::Artist);
    }

    #[test]
    fn recents() {
        let cards = parse_recents(&fixture("recents.json")).unwrap();
        assert_eq!(cards.len(), 4);
        assert_eq!(cards[0].uri, "spotify:collection:tracks");
        assert_eq!(cards[0].title, "Liked Songs");
        assert_eq!(cards[0].kind, CardKind::Playlist);
        assert_eq!(cards[0].images.len(), 1);
        assert_eq!(cards[0].images[0].width, None);
        assert_eq!(cards[1].title, "nerve");
        assert_eq!(cards[1].kind, CardKind::Track);
        assert_eq!(cards[1].subtitle, "2hollis");
        assert_eq!(cards[1].images.len(), 3);
        assert_eq!(cards[1].images[0].width, Some(640));
    }

    #[test]
    fn recents_kinds() {
        let mut body = fixture("recents.json");
        let items = body
            .pointer_mut("/data/lists/0/items/items")
            .and_then(Value::as_array_mut)
            .unwrap();
        for (item, kind) in items.iter_mut().zip([
            "ENTITY_TYPE_ALBUM",
            "ENTITY_TYPE_ARTIST",
            "ENTITY_TYPE_SOMETHING_NEW",
        ]) {
            item["entity"]["data"]["entityTypeTrait"]["type"] = json!(kind);
        }
        items[3]["entity"]["_uri"] = Value::Null;
        let kinds: Vec<CardKind> = parse_recents(&body)
            .unwrap()
            .iter()
            .map(|c| c.kind)
            .collect();
        assert_eq!(kinds, [CardKind::Album, CardKind::Artist, CardKind::Other]);
    }

    #[test]
    fn in_library() {
        assert_eq!(
            parse_in_library(&fixture("in_library.json")).unwrap(),
            [true]
        );
        let mixed = json!({"data": {"lookup": [
            {"data": {"saved": false}},
            {"data": {"__typename": "NotFound"}},
            {"data": {"saved": true}},
        ]}});
        assert_eq!(parse_in_library(&mixed).unwrap(), [false, false, true]);
    }

    #[test]
    fn lyrics() {
        let lyrics = parse_lyrics(&fixture("lyrics.json")).unwrap();
        assert!(lyrics.synced);
        assert_eq!(lyrics.provider, "MusixMatch");
        assert_eq!(lyrics.lines.len(), 5);
        assert_eq!(
            lyrics.lines[0],
            LyricLine {
                start_ms: 2850,
                words: "You're just jealous".into()
            }
        );
        assert_eq!(lyrics.lines[4].start_ms, 24100);
    }

    #[test]
    fn lyrics_unsynced() {
        let mut body = fixture("lyrics.json");
        body["lyrics"]["syncType"] = json!("UNSYNCED");
        let lyrics = parse_lyrics(&body).unwrap();
        assert!(!lyrics.synced);
        assert_eq!(lyrics.lines.len(), 5);
    }

    #[test]
    fn credits() {
        let roles = parse_credits(&fixture("credits.json")).unwrap();
        let names: Vec<(&str, Vec<&str>)> = roles
            .iter()
            .map(|r| {
                (
                    r.role.as_str(),
                    r.names.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("Performers", vec!["Jane Remover"]),
                ("Writers", vec!["Jane Remover"]),
                ("Producers", vec!["Jane Remover"]),
            ]
        );
    }

    #[test]
    fn credits_drop_roles_without_names() {
        let mut body = fixture("credits.json");
        body["roleCredits"][1]["artists"] = json!([]);
        body["roleCredits"][2]["artists"] = json!([{"uri": "spotify:artist:x"}]);
        let roles = parse_credits(&body).unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].role, "Performers");
    }

    #[test]
    fn radio() {
        assert_eq!(
            parse_radio(&fixture("radio.json")).as_deref(),
            Some("spotify:playlist:37i9dQZF1E8PRJX8bAPdG3")
        );
        assert_eq!(parse_radio(&json!({"total": 0, "mediaItems": []})), None);
        assert_eq!(parse_radio(&json!({})), None);
    }

    #[test]
    fn new_pages_missing_top_level_is_unexpected_page() {
        let empty = json!({"data": {}});
        let unexpected = |r: Result<()>| matches!(r, Err(SpotError::UnexpectedPage(_)));
        assert!(unexpected(parse_home(&empty).map(drop)));
        assert!(unexpected(parse_search(&empty).map(drop)));
        assert!(unexpected(parse_artist(&empty).map(drop)));
        assert!(unexpected(
            parse_artist(&json!({"data": {"artistUnion": {"__typename": "NotFound"}}})).map(drop)
        ));
        assert!(unexpected(parse_recents(&empty).map(drop)));
        assert!(unexpected(
            parse_recents(&json!({"data": {"lists": []}})).map(drop)
        ));
        assert!(unexpected(parse_in_library(&empty).map(drop)));
        assert!(unexpected(parse_lyrics(&json!({})).map(drop)));
        assert!(unexpected(parse_credits(&json!({})).map(drop)));
    }
}
