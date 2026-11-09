//! Scrapes persisted-query hashes from the public web player. Its index page names
//! the bundle; the bundle holds most ops and the webpack chunk map; the route chunks
//! hold the rest (search lives in `xpui-routes-search`). Every request is public (no
//! credentials) and goes on the Background lane. The index page is asked for as
//! [`INDEX_USER_AGENT`], so it names the desktop player; the scripts get the plain
//! [`super::USER_AGENT`].

use std::{collections::BTreeMap, sync::LazyLock};

use regex::Regex;

use super::{INDEX_USER_AGENT, Web, lanes::Lane, ops::Op};
use crate::{Result, SpotError};

/// At most this many chunks are fetched by one scrape.
pub const MAX_CHUNKS: usize = 40;

static SCRIPT_SRC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<script\b[^>]*?\bsrc=["']([^"']+)["']"#).unwrap());
static BUNDLE_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/web-player\.[0-9a-f]+\.js$").unwrap());
/// The loader `.u=e=>""+(({<id>:"<name>",…})[e]||e)+"."+({<id>:"<hash8>",…})[e]+".js"`;
/// the minifier may drop the parentheses around either map or rename `e`.
static CHUNK_LOADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"\.u=[\w$]+=>""\+\(*\{([^{}]*)\}\)*\[[\w$]+\]\|\|[\w$]+\)"#,
        r#"\+"\."\+\(*\{([^{}]*)\}\)*\[[\w$]+\]\+"\.js""#,
    ))
    .unwrap()
});
static MAP_ENTRY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""?([\w$-]+)"?:"([^"]*)""#).unwrap());
static PERSISTED_OP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""([A-Za-z0-9_]+)","(?:query|mutation)","([0-9a-f]{64})""#).unwrap()
});

/// The `<script src>` of the web-player bundle in the index page, as written there.
pub fn web_player_url(index_html: &str) -> Option<String> {
    SCRIPT_SRC
        .captures_iter(index_html)
        .map(|c| c[1].to_owned())
        .find(|src| BUNDLE_FILE.is_match(src))
}

/// The named chunks in the bundle's chunk map, as URLs under `chunk_base`: names
/// containing `search` first, then the other `xpui-routes-*`, then the other
/// `xpui-root-*`, each group in map order. Unnamed chunks and other names are left
/// out; so is the whole map if the bundle has no loader of the known shape.
pub fn chunk_urls(bundle_js: &str, chunk_base: &str) -> Vec<String> {
    let Some(loader) = CHUNK_LOADER.captures(bundle_js) else {
        return Vec::new();
    };
    let entries = |map: &str| -> Vec<(String, String)> {
        MAP_ENTRY
            .captures_iter(map)
            .map(|c| (c[1].to_owned(), c[2].to_owned()))
            .collect()
    };
    let hashes: BTreeMap<String, String> = entries(&loader[2]).into_iter().collect();
    let mut named: Vec<(u8, String)> = entries(&loader[1])
        .into_iter()
        .filter_map(|(id, name)| {
            let group = if name.contains("search") {
                0
            } else if name.starts_with("xpui-routes-") {
                1
            } else if name.starts_with("xpui-root-") {
                2
            } else {
                return None;
            };
            let hash = hashes.get(&id)?;
            Some((group, format!("{chunk_base}{name}.{hash}.js")))
        })
        .collect();
    // Stable: map order within each group.
    named.sort_by_key(|(group, _)| *group);
    named.into_iter().map(|(_, url)| url).collect()
}

/// Every persisted query in `js`: op name to sha256. A name seen twice keeps its
/// first hash.
pub fn ops_in(js: &str) -> BTreeMap<String, String> {
    let mut ops = BTreeMap::new();
    for c in PERSISTED_OP.captures_iter(js) {
        ops.entry(c[1].to_owned())
            .or_insert_with(|| c[2].to_owned());
    }
    ops
}

/// Scrapes the hashes of `wanted`: the index page, the bundle, then chunks in
/// [`chunk_urls`] order until every wanted op is found or [`MAX_CHUNKS`] were fetched.
/// Returns what was found of `wanted` only, possibly not all of it. A chunk that
/// answers an error status is skipped; any other failure ends the scrape.
pub async fn scrape(web: &Web, wanted: &[Op]) -> Result<BTreeMap<String, String>> {
    let site = &web.bases().public_site;
    let index = web
        .public_get_text_as(Lane::Background, site, INDEX_USER_AGENT)
        .await?;
    let src = web_player_url(&index)
        .ok_or_else(|| SpotError::Scrape("the index page names no web-player bundle".into()))?;
    // The page's URLs are absolute today; a relative one is taken against the site.
    let bundle_url = reqwest::Url::parse(site)
        .and_then(|site| site.join(&src))
        .map_err(|e| SpotError::Scrape(format!("bad web-player bundle URL: {e}")))?;
    let bundle = web
        .public_get_text(Lane::Background, bundle_url.as_str())
        .await?;

    let mut found = BTreeMap::new();
    let mut take = |js: &str| {
        for (name, hash) in ops_in(js) {
            if wanted.iter().any(|op| op.name() == name) {
                found.entry(name).or_insert(hash);
            }
        }
        wanted.iter().all(|op| found.contains_key(op.name()))
    };
    let mut requests = 2;
    let mut complete = take(&bundle);
    for url in chunk_urls(&bundle, &web.bases().chunk_base)
        .iter()
        .take(MAX_CHUNKS)
    {
        if complete {
            break;
        }
        requests += 1;
        match web.public_get_text(Lane::Background, url).await {
            Ok(js) => complete = take(&js),
            Err(e @ SpotError::Http { .. }) => log::debug!("skipping a web-player chunk: {e}"),
            Err(e) => return Err(e),
        }
    }
    log::debug!(
        "hash scrape: {} of {} wanted ops in {requests} requests",
        found.len(),
        wanted.len()
    );
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{any, path},
    };

    use super::*;
    use crate::web::{
        INDEX_USER_AGENT, USER_AGENT,
        test_support::{INDEX_HTML, ROUTE_SEARCH_JS, WEB_PLAYER_JS, fixture, mount_site},
    };

    const FETCH_PLAYLIST: &str = "21ea11a6ac75a5f3e01fd365d29c18e0a4a8b7dc09f89bbb5d73910190c6acaf";
    const ADD_TO_LIBRARY: &str = "896ebcb47815681340860d121cb5d494e157e2a78d3950385cd54e0393c67148";
    const SEARCH_DESKTOP: &str = "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03";

    fn paths_requested(requests: &[wiremock::Request]) -> Vec<String> {
        requests.iter().map(|r| r.url.path().to_string()).collect()
    }

    #[test]
    fn finds_web_player_script() {
        assert_eq!(
            web_player_url(INDEX_HTML).as_deref(),
            Some("https://open.spotifycdn.com/cdn/build/web-player/web-player.73b73c08.js")
        );
    }

    #[test]
    fn no_web_player_script() {
        // The mobile player Spotify serves to unknown user-agents, and a near miss.
        let mobile = r#"<script src="https://open.spotifycdn.com/cdn/build/mobile-web-player/mobile-web-player.eae5fa6b.js"></script>"#;
        assert_eq!(web_player_url(mobile), None);
        assert_eq!(
            web_player_url(r#"<script src="/x/web-player.73b73c08.js.map"></script>"#),
            None
        );
        assert_eq!(web_player_url("<html></html>"), None);
    }

    #[test]
    fn chunk_urls_named_search_first() {
        assert_eq!(
            chunk_urls(WEB_PLAYER_JS, "B/"),
            [
                "B/xpui-routes-search.a89e24fe.js",
                "B/xpui-routes-playlist.dbae6631.js",
                "B/xpui-root-dialogs.fa6f4ac8.js",
            ]
        );
    }

    #[test]
    fn chunk_urls_other_loader_shapes() {
        // No parentheses around the maps, another argument name, and a chunk that is
        // neither search, a route nor a root (left out).
        let js = r#"r.u=t=>""+({7:"xpui-root-a",8:"vendor~x",9:"xpui-routes-b",3:"xpui-desktop-search"}[t]||t)+"."+{7:"aaaaaaaa",8:"bbbbbbbb",9:"cccccccc",3:"dddddddd"}[t]+".js""#;
        assert_eq!(
            chunk_urls(js, "B/"),
            [
                "B/xpui-desktop-search.dddddddd.js",
                "B/xpui-routes-b.cccccccc.js",
                "B/xpui-root-a.aaaaaaaa.js",
            ]
        );
        assert!(chunk_urls("no loader here", "B/").is_empty());
    }

    #[test]
    fn ops_in_bundle() {
        let bundle = ops_in(WEB_PLAYER_JS);
        assert_eq!(bundle.len(), 2, "{bundle:?}");
        assert_eq!(bundle["fetchPlaylist"], FETCH_PLAYLIST);
        assert_eq!(bundle["addToLibrary"], ADD_TO_LIBRARY);
        let search = ops_in(ROUTE_SEARCH_JS);
        assert_eq!(search.len(), 1, "{search:?}");
        assert_eq!(search["searchDesktop"], SEARCH_DESKTOP);
    }

    #[tokio::test]
    async fn scrape_walks_chunks() {
        let server = MockServer::start().await;
        mount_site(&server).await;
        let gap = Duration::from_millis(50);
        let f = fixture(&server, gap).await;

        let start = Instant::now();
        let found = scrape(&f.web, &[Op::SearchDesktop, Op::FetchPlaylist])
            .await
            .unwrap();
        assert_eq!(
            found,
            BTreeMap::from([
                ("fetchPlaylist".to_string(), FETCH_PLAYLIST.to_string()),
                ("searchDesktop".to_string(), SEARCH_DESKTOP.to_string()),
            ]),
            "only the wanted ops"
        );
        assert_eq!(
            paths_requested(&server.received_requests().await.unwrap()),
            [
                "/",
                "/cdn/build/web-player/web-player.73b73c08.js",
                "/cdn/build/web-player/xpui-routes-search.a89e24fe.js",
            ],
            "stops once every wanted op is found"
        );
        // Three requests a gap apart: every one went on the Background lane.
        assert!(start.elapsed() >= 2 * gap, "{:?}", start.elapsed());
    }

    #[tokio::test]
    async fn scrape_stops_after_the_bundle() {
        let server = MockServer::start().await;
        mount_site(&server).await;
        let f = fixture(&server, Duration::ZERO).await;

        let found = scrape(&f.web, &[Op::FetchPlaylist]).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn scrape_skips_missing_chunks() {
        // RemoveFromLibrary is in no fixture: every chunk is tried, the two the mock
        // doesn't serve (404) are skipped, and what was found is returned.
        let server = MockServer::start().await;
        mount_site(&server).await;
        let f = fixture(&server, Duration::ZERO).await;

        let found = scrape(&f.web, &[Op::SearchDesktop, Op::RemoveFromLibrary])
            .await
            .unwrap();
        assert_eq!(
            found,
            BTreeMap::from([("searchDesktop".to_string(), SEARCH_DESKTOP.to_string())])
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn scrape_caps_chunks() {
        let server = MockServer::start().await;
        // A bundle naming 50 route chunks, none of which holds the wanted op.
        let names: Vec<String> = (0..50)
            .map(|i| format!("{i}:\"xpui-routes-r{i}\""))
            .collect();
        let hashes: Vec<String> = (0..50).map(|i| format!("{i}:\"{i:08x}\"")).collect();
        let bundle = format!(
            r#"a.u=e=>""+(({{{}}})[e]||e)+"."+({{{}}})[e]+".js";"#,
            names.join(","),
            hashes.join(",")
        );
        let index = format!(
            r#"<script src="{}/cdn/build/web-player/web-player.0123abcd.js"></script>"#,
            server.uri()
        );
        Mock::given(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(index))
            .mount(&server)
            .await;
        Mock::given(path("/cdn/build/web-player/web-player.0123abcd.js"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bundle))
            .mount(&server)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string("x"))
            .with_priority(10)
            .mount(&server)
            .await;
        let f = fixture(&server, Duration::ZERO).await;

        let found = scrape(&f.web, &[Op::Home]).await.unwrap();
        assert!(found.is_empty());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2 + MAX_CHUNKS
        );
    }

    #[tokio::test]
    async fn scrape_without_bundle_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>"))
            .mount(&server)
            .await;
        let f = fixture(&server, Duration::ZERO).await;

        let err = scrape(&f.web, &Op::ALL).await.unwrap_err();
        assert!(matches!(err, SpotError::Scrape(_)), "{err:?}");
    }

    fn user_agent(req: &wiremock::Request) -> &str {
        req.headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }

    #[test]
    fn index_user_agent_is_ours_and_desktop() {
        assert!(INDEX_USER_AGENT.starts_with(&format!("{USER_AGENT} ")));
        assert!(INDEX_USER_AGENT.contains("Gecko/20100101 Firefox/"));
    }

    #[tokio::test]
    async fn index_asks_for_the_desktop_player() {
        let server = MockServer::start().await;
        mount_site(&server).await;
        let f = fixture(&server, Duration::ZERO).await;

        scrape(&f.web, &[Op::SearchDesktop]).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let (index, scripts) = requests.split_first().unwrap();
        assert_eq!(index.url.path(), "/");
        assert!(user_agent(index).starts_with("LOUDAUTO/"));
        assert!(user_agent(index).contains("Firefox/"));
        assert_eq!(user_agent(index), INDEX_USER_AGENT);
        for script in scripts {
            assert_eq!(user_agent(script), USER_AGENT, "{}", script.url.path());
        }
    }
}
