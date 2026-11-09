//! The pages, heart, lyrics, credits and radio, fetched from Spotify's web-player
//! endpoints. Every method but [`Web::set_liked`] takes its lane first.
//!
//! A track list that spans pages (album, playlist) is fetched whole. Paging counts the
//! raw items of each page, not the rows parsed from them: the parsers skip rows that
//! aren't playable tracks, so a page of only skipped rows is not the end of the list.

use std::path::Path;

use librespot_core::SpotifyUri;
use serde_json::{Value, json};

use super::{Web, lanes::Lane, ops::Op};
use crate::{
    Result, SpotError,
    pages::{
        AlbumPage, ArtistPage, Card, CreditRole, Home, LikedPage, Lyrics, Named, PlaylistPage,
        SearchResults, parse_album, parse_album_tracks, parse_artist, parse_credits, parse_home,
        parse_in_library, parse_liked, parse_lyrics, parse_playlist, parse_radio, parse_recents,
        parse_search,
    },
};

/// Tracks per `getAlbum` and `queryAlbumTracks` page.
const ALBUM_LIMIT: u32 = 50;
/// Where an album body holds its page of raw track items.
const ALBUM_ITEMS: &str = "/data/albumUnion/tracksV2/items";
/// Rows per `fetchPlaylist` page.
const PLAYLIST_LIMIT: u32 = 100;
/// Where a playlist body holds its page of raw items.
const PLAYLIST_ITEMS: &str = "/data/playlistV2/content/items";
/// Rows per [`Web::liked_page`]: the next page starts this much further on.
pub const LIKED_PAGE_SIZE: u32 = 50;

impl Web {
    /// Spotify's Home, for the local time zone.
    pub async fn home(&self, lane: Lane) -> Result<Home> {
        let vars = json!({
            "timeZone": local_time_zone().await,
            "sp_t": "",
            "facet": "",
            "sectionItemsLimit": 10,
            "homeEndUserIntegration": "INTEGRATION_WEB_PLAYER",
            "includeHomeChapterVideoCards": false,
            "includeEpisodeContentRatingsV2": false,
        });
        parse_home(&self.query(lane, Op::Home, vars).await?)
    }

    /// The first page of results for `query`, trimmed. A blank query is `EmptyQuery`, and
    /// nothing is sent.
    pub async fn search(&self, lane: Lane, query: &str) -> Result<SearchResults> {
        let query = query.trim();
        if query.is_empty() {
            return Err(SpotError::EmptyQuery);
        }
        let vars = json!({
            "searchTerm": query,
            "offset": 0,
            "limit": 10,
            "numberOfTopResults": 5,
            "includeAudiobooks": false,
            "includeArtistHasConcertsField": false,
            "includePreReleases": true,
            "includeLocalConcertsField": false,
            "includeAuthors": false,
        });
        parse_search(&self.query(lane, Op::SearchDesktop, vars).await?)
    }

    pub async fn artist(&self, lane: Lane, uri: &str) -> Result<ArtistPage> {
        let vars = json!({"uri": uri, "locale": "", "includePrerelease": true});
        parse_artist(&self.query(lane, Op::ArtistOverview, vars).await?)
    }

    /// An album with all its tracks: `getAlbum`, then `queryAlbumTracks` for each later
    /// page, whose rows get the album filled in.
    pub async fn album(&self, lane: Lane, uri: &str) -> Result<AlbumPage> {
        let vars = json!({"uri": uri, "locale": "", "offset": 0, "limit": ALBUM_LIMIT});
        let body = self.query(lane, Op::GetAlbum, vars).await?;
        let mut page = parse_album(&body)?;
        let album = Named {
            uri: page.uri.clone(),
            name: page.name.clone(),
        };
        let lane = follow_up(lane);
        let mut pager = Pager::new(ALBUM_LIMIT, page.total);
        let mut raw = raw_len(&body, ALBUM_ITEMS);
        while let Some(offset) = pager.next(raw) {
            let vars = json!({"uri": uri, "offset": offset, "limit": ALBUM_LIMIT});
            let body = self.query(lane, Op::AlbumTracks, vars).await?;
            raw = raw_len(&body, ALBUM_ITEMS);
            page.tracks
                .extend(parse_album_tracks(&body)?.into_iter().map(|mut row| {
                    row.album = Some(album.clone());
                    row
                }));
        }
        Ok(page)
    }

    /// A playlist with all its rows, `fetchPlaylist` a page at a time.
    pub async fn playlist(&self, lane: Lane, uri: &str) -> Result<PlaylistPage> {
        let vars = |offset: u32| {
            json!({
                "uri": uri,
                "offset": offset,
                "limit": PLAYLIST_LIMIT,
                "enableWatchFeedEntrypoint": false,
            })
        };
        let body = self.query(lane, Op::FetchPlaylist, vars(0)).await?;
        let mut page = parse_playlist(&body)?;
        let lane = follow_up(lane);
        let mut pager = Pager::new(PLAYLIST_LIMIT, page.total);
        let mut raw = raw_len(&body, PLAYLIST_ITEMS);
        while let Some(offset) = pager.next(raw) {
            let body = self.query(lane, Op::FetchPlaylist, vars(offset)).await?;
            raw = raw_len(&body, PLAYLIST_ITEMS);
            page.tracks.extend(parse_playlist(&body)?.tracks);
        }
        Ok(page)
    }

    /// The page of Liked Songs that starts at `offset`. The next page starts at
    /// `offset + LIKED_PAGE_SIZE`, not `offset + tracks.len()` (rows that aren't
    /// playable tracks are skipped), and the list ends at `total`.
    pub async fn liked_page(&self, lane: Lane, offset: u32) -> Result<LikedPage> {
        let vars = json!({"offset": offset, "limit": LIKED_PAGE_SIZE});
        parse_liked(&self.query(lane, Op::LibraryTracks, vars).await?)
    }

    /// Recently played.
    pub async fn recents(&self, lane: Lane) -> Result<Vec<Card>> {
        let vars = json!({"uris": ["spotify:list:recents:page"], "offset": 0, "limit": 50});
        parse_recents(&self.query(lane, Op::Recents, vars).await?)
    }

    /// Whether each of `uris` is in the library, in the same order. No URIs send
    /// nothing; an answer that doesn't match the URIs one to one is `UnexpectedPage`.
    pub async fn in_library(&self, lane: Lane, uris: &[String]) -> Result<Vec<bool>> {
        if uris.is_empty() {
            return Ok(Vec::new());
        }
        let body = self
            .query(lane, Op::InLibrary, json!({"uris": uris}))
            .await?;
        let saved = parse_in_library(&body)?;
        if saved.len() != uris.len() {
            return Err(SpotError::UnexpectedPage(format!(
                "areEntitiesInLibrary ({} answers for {} URIs)",
                saved.len(),
                uris.len()
            )));
        }
        Ok(saved)
    }

    /// Hearts one track (`liked`) or takes it out of Liked Songs, on the User lane. It is
    /// the only write to the account, and only ever for one track the user chose: any
    /// URI but a track's is refused with nothing sent (`InvalidUri`), and the track goes out in its
    /// plain `spotify:track:<id>` form.
    pub async fn set_liked(&self, uri: &str, liked: bool) -> Result<()> {
        let uri = format!("spotify:track:{}", track_id(uri)?);
        let (op, vars) = if liked {
            (
                Op::AddToLibrary,
                json!({"libraryItemUris": [uri], "interactionId": null}),
            )
        } else {
            (Op::RemoveFromLibrary, json!({"libraryItemUris": [uri]}))
        };
        self.query(Lane::User, op, vars).await?;
        Ok(())
    }

    /// A track's lyrics, or `None` when it has none: a 404, or an answer without lines.
    pub async fn lyrics(&self, lane: Lane, track_uri: &str) -> Result<Option<Lyrics>> {
        let id = track_id(track_uri)?;
        let path = format!("/color-lyrics/v2/track/{id}?format=json&vocalRemoval=false");
        let Some(body) = self.spclient_get(lane, &path).await? else {
            return Ok(None);
        };
        if !body.get("lyrics").is_some_and(Value::is_object) {
            return Ok(None);
        }
        let lyrics = parse_lyrics(&body)?;
        Ok((!lyrics.lines.is_empty()).then_some(lyrics))
    }

    /// A track's credits by role; none when Spotify has no credits for it (a 404).
    pub async fn credits(&self, lane: Lane, track_uri: &str) -> Result<Vec<CreditRole>> {
        let id = track_id(track_uri)?;
        let path = format!("/track-credits-view/v0/experimental/{id}/credits");
        match self.spclient_get(lane, &path).await? {
            Some(body) => parse_credits(&body),
            None => Ok(Vec::new()),
        }
    }

    /// The URI of the radio playlist Spotify makes for a track or an artist. Any other
    /// seed is refused with nothing sent (`InvalidUri`); an answer without a playlist
    /// (or a 404) is `UnexpectedPage`.
    pub async fn radio_for(&self, lane: Lane, seed_uri: &str) -> Result<String> {
        let seed = match SpotifyUri::from_uri(seed_uri) {
            Ok(uri @ (SpotifyUri::Track { .. } | SpotifyUri::Artist { .. })) => uri.to_uri(),
            _ => {
                return Err(SpotError::InvalidUri(format!(
                    "{seed_uri} isn't a track or an artist"
                )));
            }
        };
        let path = format!("/inspiredby-mix/v2/seed_to_playlist/{seed}?response-format=json");
        self.spclient_get(lane, &path)
            .await?
            .as_ref()
            .and_then(parse_radio)
            .ok_or_else(|| {
                SpotError::UnexpectedPage(format!(
                    "inspiredby-mix for {seed} (no mediaItems.0.uri)"
                ))
            })
    }
}

/// Offset paging over a list of `total` items read `limit` at a time, from offset 0.
/// It goes by the raw items each page held, never by the rows parsed from them.
struct Pager {
    limit: u32,
    total: u32,
    offset: u32,
}

impl Pager {
    fn new(limit: u32, total: u32) -> Self {
        Pager {
            limit,
            total,
            offset: 0,
        }
    }

    /// After a page that held `raw` items, the offset of the next page; `None` once
    /// the offset reaches `total`, or after an empty page.
    fn next(&mut self, raw: usize) -> Option<u32> {
        if raw == 0 {
            return None;
        }
        self.offset = self.offset.saturating_add(self.limit);
        (self.offset < self.total).then_some(self.offset)
    }
}

/// How many raw items the array at `pointer` holds (0 when absent).
fn raw_len(body: &Value, pointer: &str) -> usize {
    body.pointer(pointer)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// The lane for a list's later pages: an admitted Prefetch finishes as Background
/// rather than being dropped halfway, as `Library::liked` does.
fn follow_up(lane: Lane) -> Lane {
    match lane {
        Lane::Prefetch => Lane::Background,
        other => other,
    }
}

/// The base62 id of a `spotify:track:` URI; anything else is `InvalidUri`, so no
/// request is made for it.
fn track_id(uri: &str) -> Result<String> {
    match SpotifyUri::from_uri(uri) {
        Ok(SpotifyUri::Track { id }) => Ok(id.to_base62()),
        _ => Err(SpotError::InvalidUri(format!("{uri} isn't a track"))),
    }
}

/// The IANA name of the local time zone, from where `/etc/localtime` links; "UTC" when
/// it isn't a link.
async fn local_time_zone() -> String {
    match tokio::fs::read_link("/etc/localtime").await {
        Ok(target) => time_zone_from_link(&target),
        Err(_) => "UTC".to_string(),
    }
}

/// The zone in a `/etc/localtime` link target: the part after `zoneinfo/` (and after
/// `posix/` or `right/`, which hold the same zones), else "UTC".
fn time_zone_from_link(target: &Path) -> String {
    let target = target.to_string_lossy();
    target
        .split_once("zoneinfo/")
        .map(|(_, zone)| {
            zone.strip_prefix("posix/")
                .or_else(|| zone.strip_prefix("right/"))
                .unwrap_or(zone)
        })
        .filter(|zone| !zone.is_empty())
        .unwrap_or("UTC")
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::{ops::Range, path::Path, sync::atomic::Ordering, time::Duration};

    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path, query_param},
    };

    use super::*;
    use crate::{
        pages::TrackRow,
        web::{lanes::BACKGROUND_GAP, test_support::fixture},
    };

    const QUERY_PATH: &str = "/pathfinder/v2/query";
    const ALBUM: &str = "spotify:album:21b4cDNse2AMpj94ykfuON";
    const ARTIST: &str = "spotify:artist:2rLGlNI6htigNxx172qxLu";
    const PLAYLIST: &str = "spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi";
    const TRACK_ID: &str = "1XD4K4CGAKTIBmFpvuaFru";
    const TRACK: &str = "spotify:track:1XD4K4CGAKTIBmFpvuaFru";

    fn captured(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/web/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    /// The whole body `op` sends with `vars`; the fixture's registry is empty, so the
    /// hash is the fallback.
    fn persisted(op: Op, vars: Value) -> Value {
        json!({
            "operationName": op.name(),
            "variables": vars,
            "extensions": {"persistedQuery": {"version": 1, "sha256Hash": op.fallback_hash()}},
        })
    }

    /// Answers `op` sent with exactly `vars` with `reply`, and expects it once.
    async fn expect_query(server: &MockServer, op: Op, vars: Value, reply: Value) {
        Mock::given(method("POST"))
            .and(path(QUERY_PATH))
            .and(body_json(persisted(op, vars)))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .expect(1)
            .mount(server)
            .await;
    }

    /// Answers a GET of `at` with `response`, and expects it once.
    async fn expect_get(server: &MockServer, at: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(response)
            .expect(1)
            .mount(server)
            .await;
    }

    async fn requests(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    fn track(n: usize) -> Value {
        json!({"uri": format!("spotify:track:t{n}"), "name": format!("song {n}")})
    }

    fn album_items(rows: Range<usize>) -> Vec<Value> {
        rows.map(|n| json!({"uid": format!("u{n}"), "track": track(n)}))
            .collect()
    }

    /// A `getAlbum` answer holding rows `rows` of an album of `total`.
    fn album_body(total: usize, rows: Range<usize>) -> Value {
        json!({"data": {"albumUnion": {
            "__typename": "Album",
            "uri": ALBUM,
            "name": "Revengeseekerz",
            "tracksV2": {"totalCount": total, "items": album_items(rows)},
        }}})
    }

    /// A `queryAlbumTracks` answer, which doesn't name the album.
    fn album_tracks_body(total: usize, rows: Range<usize>) -> Value {
        json!({"data": {"albumUnion": {
            "__typename": "Album",
            "tracksV2": {"totalCount": total, "items": album_items(rows)},
        }}})
    }

    fn album_vars(offset: u32) -> Value {
        json!({"uri": ALBUM, "offset": offset, "limit": 50})
    }

    fn playlist_track(n: usize) -> Value {
        json!({"uid": format!("p{n}"), "itemV2": {"data": track(n)}})
    }

    /// A row the parser skips: an episode.
    fn playlist_episode(n: usize) -> Value {
        json!({"uid": format!("e{n}"), "itemV2": {"data": {
            "__typename": "Episode",
            "uri": format!("spotify:episode:e{n}"),
            "name": format!("episode {n}"),
        }}})
    }

    /// A `fetchPlaylist` answer holding `items` of a playlist of `total`.
    fn playlist_body(total: usize, items: Vec<Value>) -> Value {
        json!({"data": {"playlistV2": {
            "__typename": "Playlist",
            "uri": PLAYLIST,
            "name": "Main",
            "content": {"totalCount": total, "items": items},
        }}})
    }

    fn playlist_vars(offset: u32) -> Value {
        json!({"uri": PLAYLIST, "offset": offset, "limit": 100, "enableWatchFeedEntrypoint": false})
    }

    fn tracks(rows: Range<usize>) -> Vec<Value> {
        rows.map(playlist_track).collect()
    }

    fn episodes(rows: Range<usize>) -> Vec<Value> {
        rows.map(playlist_episode).collect()
    }

    fn uids(rows: &[TrackRow]) -> Vec<String> {
        rows.iter().filter_map(|r| r.uid.clone()).collect()
    }

    fn unexpected(result: Result<impl std::fmt::Debug>) {
        assert!(
            matches!(result, Err(SpotError::UnexpectedPage(_))),
            "{result:?}"
        );
    }

    /// Refused before anything was sent: a uri of the wrong kind.
    fn refused(result: Result<impl std::fmt::Debug>) {
        assert!(
            matches!(result, Err(SpotError::InvalidUri(_))),
            "{result:?}"
        );
    }

    #[test]
    fn time_zone_from_link_cases() {
        for (target, zone) in [
            ("/usr/share/zoneinfo/America/New_York", "America/New_York"),
            ("../usr/share/zoneinfo/Europe/London", "Europe/London"),
            ("/usr/share/zoneinfo/posix/Asia/Tokyo", "Asia/Tokyo"),
            ("/usr/share/zoneinfo/right/Asia/Tokyo", "Asia/Tokyo"),
            ("/usr/share/zoneinfo/UTC", "UTC"),
            ("/usr/share/zoneinfo/", "UTC"),
            ("/etc/my-localtime", "UTC"),
        ] {
            assert_eq!(time_zone_from_link(Path::new(target)), zone, "{target}");
        }
    }

    #[test]
    fn pager_counts_raw_items() {
        // 250 items, 100 a page: offsets 100 and 200, then done.
        let mut pager = Pager::new(100, 250);
        assert_eq!(pager.next(100), Some(100));
        // A page of only skipped rows still has raw items: paging goes on.
        assert_eq!(pager.next(100), Some(200));
        assert_eq!(pager.next(50), None);

        // An empty page ends the list early.
        let mut pager = Pager::new(100, 300);
        assert_eq!(pager.next(100), Some(100));
        assert_eq!(pager.next(0), None);

        // A list that fits the first page.
        assert_eq!(Pager::new(50, 50).next(50), None);
        assert_eq!(Pager::new(50, 0).next(0), None);
    }

    #[tokio::test]
    async fn home_sends_appendix_vars() {
        let server = MockServer::start().await;
        let vars = json!({
            "timeZone": local_time_zone().await,
            "sp_t": "",
            "facet": "",
            "sectionItemsLimit": 10,
            "homeEndUserIntegration": "INTEGRATION_WEB_PLAYER",
            "includeHomeChapterVideoCards": false,
            "includeEpisodeContentRatingsV2": false,
        });
        expect_query(&server, Op::Home, vars, captured("home.json")).await;
        let f = fixture(&server, Duration::ZERO).await;

        let home = f.web.home(Lane::User).await.unwrap();
        assert_eq!(home.greeting, "Good evening");
        server.verify().await;
    }

    #[tokio::test]
    async fn search_sends_vars() {
        let server = MockServer::start().await;
        let vars = json!({
            "searchTerm": "jane remover",
            "offset": 0,
            "limit": 10,
            "numberOfTopResults": 5,
            "includeAudiobooks": false,
            "includeArtistHasConcertsField": false,
            "includePreReleases": true,
            "includeLocalConcertsField": false,
            "includeAuthors": false,
        });
        expect_query(&server, Op::SearchDesktop, vars, captured("search.json")).await;
        let f = fixture(&server, Duration::ZERO).await;

        let results = f.web.search(Lane::User, " jane remover ").await.unwrap();
        assert_eq!(results.tracks[0].title, "How to Teleport");
        server.verify().await;
    }

    #[tokio::test]
    async fn search_blank_sends_nothing() {
        let server = MockServer::start().await;
        let f = fixture(&server, Duration::ZERO).await;

        for query in ["", "   ", "\t\n"] {
            let got = f.web.search(Lane::User, query).await;
            assert!(
                matches!(got, Err(SpotError::EmptyQuery)),
                "{query:?}: {got:?}"
            );
        }
        assert_eq!(requests(&server).await, 0);
        assert_eq!(f.tokens.bearer_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn artist_sends_vars() {
        let server = MockServer::start().await;
        let vars = json!({"uri": ARTIST, "locale": "", "includePrerelease": true});
        expect_query(&server, Op::ArtistOverview, vars, captured("artist.json")).await;
        let f = fixture(&server, Duration::ZERO).await;

        let artist = f.web.artist(Lane::User, ARTIST).await.unwrap();
        assert_eq!(artist.name, "Jane Remover");
        server.verify().await;
    }

    #[tokio::test]
    async fn album_pages_tracks() {
        let server = MockServer::start().await;
        let first = json!({"uri": ALBUM, "locale": "", "offset": 0, "limit": 50});
        expect_query(&server, Op::GetAlbum, first, album_body(60, 0..50)).await;
        expect_query(
            &server,
            Op::AlbumTracks,
            album_vars(50),
            album_tracks_body(60, 50..60),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let page = f.web.album(Lane::User, ALBUM).await.unwrap();
        assert_eq!(page.total, 60);
        assert_eq!(page.tracks.len(), 60);
        assert_eq!(
            uids(&page.tracks),
            (0..60).map(|n| format!("u{n}")).collect::<Vec<_>>()
        );
        let album = Some(Named {
            uri: ALBUM.into(),
            name: "Revengeseekerz".into(),
        });
        assert!(
            page.tracks.iter().all(|row| row.album == album),
            "later pages' rows name the album"
        );
        assert_eq!(requests(&server).await, 2);
        server.verify().await;
    }

    #[tokio::test]
    async fn album_on_one_page_is_one_request() {
        let server = MockServer::start().await;
        let first = json!({"uri": ALBUM, "locale": "", "offset": 0, "limit": 50});
        expect_query(&server, Op::GetAlbum, first, captured("album.json")).await;
        let f = fixture(&server, Duration::ZERO).await;

        let page = f.web.album(Lane::User, ALBUM).await.unwrap();
        assert_eq!(page.tracks.len(), 12);
        assert_eq!(requests(&server).await, 1);
    }

    #[tokio::test]
    async fn album_prefetch_finishes_on_background() {
        let server = MockServer::start().await;
        let first = json!({"uri": ALBUM, "locale": "", "offset": 0, "limit": 50});
        expect_query(&server, Op::GetAlbum, first, album_body(60, 0..50)).await;
        expect_query(
            &server,
            Op::AlbumTracks,
            album_vars(50),
            album_tracks_body(60, 50..60),
        )
        .await;
        // The second page comes inside the gap: as a Prefetch it would be dropped.
        let f = fixture(&server, BACKGROUND_GAP).await;

        let page = f.web.album(Lane::Prefetch, ALBUM).await.unwrap();
        assert_eq!(page.tracks.len(), 60);
        server.verify().await;
    }

    #[tokio::test]
    async fn playlist_pages_until_total() {
        let server = MockServer::start().await;
        expect_query(
            &server,
            Op::FetchPlaylist,
            playlist_vars(0),
            playlist_body(150, tracks(0..100)),
        )
        .await;
        expect_query(
            &server,
            Op::FetchPlaylist,
            playlist_vars(100),
            playlist_body(150, tracks(100..150)),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let page = f.web.playlist(Lane::User, PLAYLIST).await.unwrap();
        assert_eq!(page.name, "Main");
        assert_eq!(page.total, 150);
        assert_eq!(
            uids(&page.tracks),
            (0..150).map(|n| format!("p{n}")).collect::<Vec<_>>()
        );
        assert_eq!(requests(&server).await, 2);
        server.verify().await;
    }

    #[tokio::test]
    async fn playlist_paging_counts_skipped_rows() {
        let server = MockServer::start().await;
        let mut first = tracks(0..60);
        first.extend(episodes(60..100));
        expect_query(
            &server,
            Op::FetchPlaylist,
            playlist_vars(0),
            playlist_body(250, first),
        )
        .await;
        // A page of nothing but skipped rows is not the end of the list.
        expect_query(
            &server,
            Op::FetchPlaylist,
            playlist_vars(100),
            playlist_body(250, episodes(100..200)),
        )
        .await;
        expect_query(
            &server,
            Op::FetchPlaylist,
            playlist_vars(200),
            playlist_body(250, tracks(200..250)),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let page = f.web.playlist(Lane::User, PLAYLIST).await.unwrap();
        let want: Vec<String> = (0..60).chain(200..250).map(|n| format!("p{n}")).collect();
        assert_eq!(uids(&page.tracks), want);
        assert_eq!(requests(&server).await, 3, "total ends the paging");
        server.verify().await;
    }

    #[tokio::test]
    async fn playlist_stops_on_an_empty_page() {
        let server = MockServer::start().await;
        expect_query(
            &server,
            Op::FetchPlaylist,
            playlist_vars(0),
            playlist_body(300, tracks(0..100)),
        )
        .await;
        expect_query(
            &server,
            Op::FetchPlaylist,
            playlist_vars(100),
            playlist_body(300, Vec::new()),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let page = f.web.playlist(Lane::User, PLAYLIST).await.unwrap();
        assert_eq!(page.tracks.len(), 100);
        assert_eq!(requests(&server).await, 2);
        server.verify().await;
    }

    #[tokio::test]
    async fn liked_page_sends_offset() {
        let server = MockServer::start().await;
        let vars = json!({"offset": 50, "limit": 50});
        expect_query(
            &server,
            Op::LibraryTracks,
            vars,
            captured("liked_tracks.json"),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let page = f.web.liked_page(Lane::User, 50).await.unwrap();
        assert_eq!(page.total, 758);
        assert_eq!(page.tracks[0].uri, TRACK);
        server.verify().await;
    }

    #[tokio::test]
    async fn recents_sends_vars() {
        let server = MockServer::start().await;
        let vars = json!({"uris": ["spotify:list:recents:page"], "offset": 0, "limit": 50});
        expect_query(&server, Op::Recents, vars, captured("recents.json")).await;
        let f = fixture(&server, Duration::ZERO).await;

        let cards = f.web.recents(Lane::User).await.unwrap();
        assert_eq!(cards.len(), 4);
        server.verify().await;
    }

    #[tokio::test]
    async fn in_library_sends_uris() {
        let server = MockServer::start().await;
        let vars = json!({"uris": [TRACK]});
        expect_query(&server, Op::InLibrary, vars, captured("in_library.json")).await;
        let f = fixture(&server, Duration::ZERO).await;

        let saved = f.web.in_library(Lane::User, &[TRACK.to_string()]).await;
        assert_eq!(saved.unwrap(), [true]);
        server.verify().await;
    }

    #[tokio::test]
    async fn in_library_of_nothing_sends_nothing() {
        let server = MockServer::start().await;
        let f = fixture(&server, Duration::ZERO).await;

        assert!(f.web.in_library(Lane::User, &[]).await.unwrap().is_empty());
        assert_eq!(requests(&server).await, 0);
    }

    #[tokio::test]
    async fn in_library_answer_per_uri() {
        let server = MockServer::start().await;
        let uris = [
            TRACK.to_string(),
            "spotify:track:4SWyOAtHVNdnczEIsLvMN2".into(),
        ];
        expect_query(
            &server,
            Op::InLibrary,
            json!({"uris": uris}),
            captured("in_library.json"),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        // One answer for two URIs can't be matched up.
        unexpected(f.web.in_library(Lane::User, &uris).await);
    }

    #[tokio::test]
    async fn set_liked_add_and_remove_bodies() {
        let server = MockServer::start().await;
        expect_query(
            &server,
            Op::AddToLibrary,
            json!({"libraryItemUris": [TRACK], "interactionId": null}),
            json!({"data": {"addLibraryItems": {"__typename": "AddLibraryItemsResponse"}}}),
        )
        .await;
        expect_query(
            &server,
            Op::RemoveFromLibrary,
            json!({"libraryItemUris": [TRACK]}),
            json!({"data": {"removeLibraryItems": {"__typename": "RemoveLibraryItemsResponse"}}}),
        )
        .await;
        // A Background request outstanding for an hour: the heart must not wait on it.
        let f = fixture(&server, Duration::from_secs(3600)).await;
        assert!(f.lanes.admit(Lane::Background).await);

        for liked in [true, false] {
            tokio::time::timeout(Duration::from_secs(5), f.web.set_liked(TRACK, liked))
                .await
                .expect("sent on the User lane")
                .unwrap();
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn set_liked_sends_the_plain_track_uri() {
        let server = MockServer::start().await;
        expect_query(
            &server,
            Op::AddToLibrary,
            json!({"libraryItemUris": [TRACK], "interactionId": null}),
            json!({"data": {"addLibraryItems": {"__typename": "AddLibraryItemsResponse"}}}),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let tagged = format!("{TRACK}:extra");
        f.web.set_liked(&tagged, true).await.unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn set_liked_refuses_anything_but_a_track() {
        let server = MockServer::start().await;
        let f = fixture(&server, Duration::ZERO).await;

        for uri in [
            ALBUM,
            PLAYLIST,
            "spotify:track:",
            "spotify:track:not/an/id",
            "nerve",
        ] {
            refused(f.web.set_liked(uri, true).await);
        }
        assert_eq!(requests(&server).await, 0);
    }

    #[tokio::test]
    async fn lyrics_of_a_track() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/color-lyrics/v2/track/{TRACK_ID}")))
            .and(query_param("format", "json"))
            .and(query_param("vocalRemoval", "false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(captured("lyrics.json")))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(&server, Duration::ZERO).await;

        let lyrics = f.web.lyrics(Lane::User, TRACK).await.unwrap().unwrap();
        assert!(lyrics.synced);
        assert_eq!(lyrics.lines.len(), 5);
        server.verify().await;
    }

    #[tokio::test]
    async fn lyrics_404_is_none() {
        let server = MockServer::start().await;
        let at = format!("/color-lyrics/v2/track/{TRACK_ID}");
        expect_get(&server, &at, ResponseTemplate::new(404)).await;
        let f = fixture(&server, Duration::ZERO).await;

        assert_eq!(f.web.lyrics(Lane::User, TRACK).await.unwrap(), None);
        server.verify().await;
    }

    #[tokio::test]
    async fn lyrics_without_lines_are_none() {
        let at = format!("/color-lyrics/v2/track/{TRACK_ID}");
        for reply in [
            ResponseTemplate::new(200).set_body_json(json!({})),
            ResponseTemplate::new(200).set_body_json(json!({"lyrics": null})),
            ResponseTemplate::new(200)
                .set_body_json(json!({"lyrics": {"syncType": "LINE_SYNCED", "lines": []}})),
            ResponseTemplate::new(200),
        ] {
            let server = MockServer::start().await;
            expect_get(&server, &at, reply).await;
            let f = fixture(&server, Duration::ZERO).await;

            assert_eq!(f.web.lyrics(Lane::User, TRACK).await.unwrap(), None);
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn lyrics_and_credits_need_a_track() {
        let server = MockServer::start().await;
        let f = fixture(&server, Duration::ZERO).await;

        let episode = "spotify:episode:5BUTPG0Csnl1fF5RFg5lUH";
        refused(f.web.lyrics(Lane::User, episode).await);
        refused(f.web.credits(Lane::User, ALBUM).await);
        assert_eq!(requests(&server).await, 0);
    }

    #[tokio::test]
    async fn credits_of_a_track() {
        let server = MockServer::start().await;
        let at = format!("/track-credits-view/v0/experimental/{TRACK_ID}/credits");
        expect_get(
            &server,
            &at,
            ResponseTemplate::new(200).set_body_json(captured("credits.json")),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let roles = f.web.credits(Lane::User, TRACK).await.unwrap();
        let titles: Vec<&str> = roles.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(titles, ["Performers", "Writers", "Producers"]);
        server.verify().await;
    }

    #[tokio::test]
    async fn credits_404_is_empty() {
        let server = MockServer::start().await;
        let at = format!("/track-credits-view/v0/experimental/{TRACK_ID}/credits");
        expect_get(&server, &at, ResponseTemplate::new(404)).await;
        let f = fixture(&server, Duration::ZERO).await;

        assert!(f.web.credits(Lane::User, TRACK).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn radio_uri() {
        for seed in [TRACK, ARTIST] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!("/inspiredby-mix/v2/seed_to_playlist/{seed}")))
                .and(query_param("response-format", "json"))
                .respond_with(ResponseTemplate::new(200).set_body_json(captured("radio.json")))
                .expect(1)
                .mount(&server)
                .await;
            let f = fixture(&server, Duration::ZERO).await;

            let radio = f.web.radio_for(Lane::User, seed).await.unwrap();
            assert_eq!(radio, "spotify:playlist:37i9dQZF1E8PRJX8bAPdG3", "{seed}");
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn radio_without_item_is_unexpected() {
        let at = format!("/inspiredby-mix/v2/seed_to_playlist/{TRACK}");
        for reply in [
            ResponseTemplate::new(200).set_body_json(json!({"total": 0, "mediaItems": []})),
            ResponseTemplate::new(404),
        ] {
            let server = MockServer::start().await;
            expect_get(&server, &at, reply).await;
            let f = fixture(&server, Duration::ZERO).await;

            unexpected(f.web.radio_for(Lane::User, TRACK).await);
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn radio_needs_a_track_or_an_artist() {
        let server = MockServer::start().await;
        let f = fixture(&server, Duration::ZERO).await;

        for seed in [ALBUM, PLAYLIST, "spotify:track:x?y", ""] {
            refused(f.web.radio_for(Lane::User, seed).await);
        }
        assert_eq!(requests(&server).await, 0);
    }
}
