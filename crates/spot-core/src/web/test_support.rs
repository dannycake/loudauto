//! Test doubles shared by the web modules' tests: fake tokens, a [`Web`] pointed at a
//! mock server, and the captured web-player assets served from it.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use super::{Bases, Web, lanes::Lanes, registry::HashRegistry, tokens::Tokens};
use crate::Result;

/// The captured public web-player index page.
pub const INDEX_HTML: &str = include_str!("../../tests/fixtures/web/index.html");
/// The captured web-player bundle (trimmed to its chunk map and two ops).
pub const WEB_PLAYER_JS: &str = include_str!("../../tests/fixtures/web/web-player.js");
/// The captured `xpui-routes-search` chunk (trimmed to its op).
pub const ROUTE_SEARCH_JS: &str = include_str!("../../tests/fixtures/web/route-search.js");

/// Where the captured index page says the web player's scripts live.
const CAPTURED_CDN: &str = "https://open.spotifycdn.com/cdn/build/web-player/";
/// The mock server's stand-in for it, under `Fixture`'s `chunk_base`.
const CHUNK_PATH: &str = "/cdn/build/web-player/";

/// Hands out bearer "b1", then "b2" on every later call, so a test can tell a
/// refreshed token from the first one.
#[derive(Default)]
pub struct FakeTokens {
    pub bearer_calls: AtomicUsize,
}

#[async_trait]
impl Tokens for FakeTokens {
    async fn bearer(&self) -> Result<String> {
        let n = self.bearer_calls.fetch_add(1, Ordering::SeqCst);
        Ok(if n == 0 { "b1" } else { "b2" }.to_string())
    }

    async fn client_token(&self) -> Result<String> {
        Ok("ct".to_string())
    }
}

pub struct Fixture {
    pub web: Web,
    pub tokens: Arc<FakeTokens>,
    pub lanes: Arc<Lanes>,
    /// Holds the registry's cache file.
    pub dir: tempfile::TempDir,
}

impl Fixture {
    /// The registry's cache file.
    pub fn registry_path(&self) -> PathBuf {
        self.dir.path().join("hashes.json")
    }
}

/// A [`Web`] with every base on `server`, Background requests `gap` apart, and an
/// empty hash registry in a temp dir.
pub async fn fixture(server: &MockServer, gap: Duration) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(HashRegistry::load(dir.path().join("hashes.json")).await);
    let tokens = Arc::new(FakeTokens::default());
    let lanes = Arc::new(Lanes::new(gap));
    let base = server.uri();
    let bases = Bases {
        spclient: base.clone(),
        pathfinder: format!("{base}/pathfinder/v2/query"),
        public_site: format!("{base}/"),
        chunk_base: format!("{base}{CHUNK_PATH}"),
    };
    let web = Web::with_bases(tokens.clone(), lanes.clone(), registry, bases);
    Fixture {
        web,
        tokens,
        lanes,
        dir,
    }
}

/// Serves the captured site from `server`: the index page (its script URLs moved onto
/// the server), the bundle and the search chunk. Any other chunk is a 404.
pub async fn mount_site(server: &MockServer) {
    let index = INDEX_HTML.replace(CAPTURED_CDN, &format!("{}{CHUNK_PATH}", server.uri()));
    for (at, body) in [
        ("/".to_string(), index),
        (
            format!("{CHUNK_PATH}web-player.73b73c08.js"),
            WEB_PLAYER_JS.to_string(),
        ),
        (
            format!("{CHUNK_PATH}xpui-routes-search.a89e24fe.js"),
            ROUTE_SEARCH_JS.to_string(),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }
}
