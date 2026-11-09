//! Pathfinder, the web player's GraphQL endpoint. The web player sends no query text,
//! only the sha256 of a query Spotify already knows; when Spotify rebuilds the player
//! the old hashes answer 412 and are scraped again from the public site.

use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{
    PATHFINDER_USER_AGENT, Web,
    client::{json_body, success},
    lanes::Lane,
    ops::Op,
    scrape::scrape,
};
use crate::{Result, SpotError};

/// After a heal scrape finds no new hash for an op, 412s on it don't scrape again for
/// this long.
pub const HEAL_COOLDOWN: Duration = Duration::from_secs(10 * 60);

impl Web {
    /// Runs `op` with `vars` and returns the whole JSON body. A 412 (Spotify's "Invalid
    /// query hash") scrapes the public web player for new hashes, saves them, and
    /// retries once with the new hash; if the scrape fails or finds no new hash for
    /// `op`, the 412 is the error, and 412s on `op` don't scrape again for
    /// [`HEAL_COOLDOWN`]. A body whose `errors` is non-empty while `data` is null is
    /// `GraphQl` with the first error's message. An `Http` error names the op after
    /// the path, as in `/pathfinder/v2/query (home)`.
    pub async fn query(&self, lane: Lane, op: Op, vars: Value) -> Result<Value> {
        self.query_unnamed(lane, op, vars)
            .await
            .map_err(|e| match e {
                SpotError::Http { status, what } => SpotError::Http {
                    status,
                    what: format!("{what} ({})", op.name()),
                },
                e => e,
            })
    }

    async fn query_unnamed(&self, lane: Lane, op: Op, vars: Value) -> Result<Value> {
        let registry = self.registry();
        // Read before the hash, so `heal` sees any scrape that finishes after it.
        let generation = registry.scrape_generation();
        let hash = registry.hash(op);
        let resp = self.post_persisted(lane, op, &vars, &hash).await?;
        if resp.status() != StatusCode::PRECONDITION_FAILED {
            return graphql_body(resp).await;
        }
        let stale = SpotError::Http {
            status: resp.status().as_u16(),
            what: resp.url().path().to_string(),
        };
        let Some(healed) = self.heal(op, &hash, generation).await else {
            return Err(stale);
        };
        let resp = self.post_persisted(lane, op, &vars, &healed).await?;
        graphql_body(resp).await
    }

    /// The engine's daily refresh: if the hashes are due, scrapes every op and saves
    /// what it found. A failed scrape, or one that found nothing, saves nothing and
    /// leaves the refresh due. If another scrape ran while this waited for the lock, it
    /// returns without scraping.
    pub async fn refresh_hashes_if_due(&self, now_ms: i64) -> Result<()> {
        let registry = self.registry();
        let generation = registry.scrape_generation();
        if !registry.is_due(now_ms) {
            return Ok(());
        }
        let _refresh = registry.refresh_lock().lock().await;
        // A 412 heal may have scraped while this waited for the lock. If it saved, the
        // refresh is no longer due; if it failed, don't repeat it straight away.
        if !registry.is_due(now_ms) || registry.scrape_generation() != generation {
            return Ok(());
        }
        self.scrape_and_save(now_ms).await.map(drop)
    }

    /// Scrapes every op now, due or not, and saves what it found; returns how many ops
    /// it found. It waits for a scrape already running, then scrapes anyway. A failed
    /// or empty scrape saves nothing, as in [`Web::refresh_hashes_if_due`].
    pub async fn refresh_hashes(&self, now_ms: i64) -> Result<usize> {
        let _refresh = self.registry().refresh_lock().lock().await;
        self.scrape_and_save(now_ms).await
    }

    /// Scrapes every op and saves what it found, or fails without saving if it found
    /// none; counts the scrape either way. Call it holding the refresh lock.
    async fn scrape_and_save(&self, now_ms: i64) -> Result<usize> {
        let registry = self.registry();
        let saved = match scrape(self, &Op::ALL).await {
            Ok(found) if !found.is_empty() => {
                log_missing(&found);
                let count = found.len();
                registry.update(found, now_ms).await.map(|()| count)
            }
            Ok(_) => Err(SpotError::Scrape(
                "the web player's scripts hold none of our ops".into(),
            )),
            Err(e) => Err(e),
        };
        registry.scrape_finished();
        saved
    }

    async fn post_persisted(
        &self,
        lane: Lane,
        op: Op,
        vars: &Value,
        hash: &str,
    ) -> Result<Response> {
        let body = json!({
            "operationName": op.name(),
            "variables": vars,
            "extensions": {"persistedQuery": {"version": 1, "sha256Hash": hash}},
        });
        let url = &self.bases().pathfinder;
        self.send_authed_as(lane, Method::POST, url, Some(&body), PATHFINDER_USER_AGENT)
            .await
    }

    /// After `stale` got a 412 for `op`: the hash to retry with, or None if there is no
    /// new one. `generation` is the registry's scrape generation from before `stale`
    /// was read. Holds the refresh lock, so concurrent 412s scrape once: a caller that
    /// waited finds the hash replaced and retries, or finds that a scrape ran meanwhile
    /// without renewing it and gives up. An op a heal scrape couldn't renew isn't
    /// scraped for again within [`HEAL_COOLDOWN`].
    async fn heal(&self, op: Op, stale: &str, generation: u64) -> Option<String> {
        let registry = self.registry();
        let _refresh = registry.refresh_lock().lock().await;
        let current = registry.hash(op);
        if current != stale {
            return Some(current);
        }
        if registry.scrape_generation() != generation {
            log::debug!(
                "{} hash refused; a scrape just ran and found no newer one",
                op.name()
            );
            return None;
        }
        if registry
            .unrenewed_at(op)
            .is_some_and(|at| at.elapsed() < HEAL_COOLDOWN)
        {
            log::debug!(
                "{} hash refused; the last scrape found no newer one, not scraping again yet",
                op.name()
            );
            return None;
        }
        // Never save a failed or empty scrape: that would postpone the daily refresh.
        match scrape(self, &Op::ALL).await {
            Ok(found) if !found.is_empty() => {
                log_missing(&found);
                // The new hashes are in use even if saving them fails.
                if let Err(e) = registry.update(found, now_ms()).await {
                    log::warn!("could not save the scraped pathfinder hashes: {e}");
                }
            }
            Ok(_) => log::warn!(
                "{} hash refused; the web player holds none of our ops",
                op.name()
            ),
            Err(e) => log::warn!("{} hash refused; could not scrape new ones: {e}", op.name()),
        }
        registry.scrape_finished();
        let healed = registry.hash(op);
        if healed == stale {
            log::warn!(
                "{} hash refused; no newer one, next scrape for it in {} min at the earliest",
                op.name(),
                HEAL_COOLDOWN.as_secs() / 60
            );
            registry.mark_unrenewed(op, Instant::now());
            return None;
        }
        Some(healed)
    }
}

/// A body whose `errors` is non-empty while `data` is null or absent is `GraphQl`;
/// anything else 2xx is returned whole.
async fn graphql_body(resp: Response) -> Result<Value> {
    let body = json_body(success(resp)?).await?;
    if let Some(first) = body
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        && body.get("data").is_none_or(Value::is_null)
    {
        let message = first
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| first.to_string(), str::to_owned);
        return Err(SpotError::GraphQl(message));
    }
    Ok(body)
}

fn log_missing(found: &BTreeMap<String, String>) {
    let missing: Vec<&str> = Op::ALL
        .iter()
        .map(|op| op.name())
        .filter(|name| !found.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        log::warn!("the web player's scripts lack hashes for {missing:?}; keeping the old ones");
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{any, body_json, body_partial_json, header, method, path},
    };

    use super::*;
    use crate::web::{
        registry::HashRegistry,
        test_support::{Fixture, fixture, mount_site},
    };

    const QUERY_PATH: &str = "/pathfinder/v2/query";
    const SEARCH_DESKTOP: &str = "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03";
    const HOUR_MS: i64 = 60 * 60 * 1000;

    fn stale() -> String {
        "ab".repeat(32)
    }

    /// Answers a POST carrying `hash` with `response`.
    async fn answer(server: &MockServer, hash: &str, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path(QUERY_PATH))
            .and(body_partial_json(
                json!({"extensions": {"persistedQuery": {"sha256Hash": hash}}}),
            ))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn invalid_hash() -> ResponseTemplate {
        ResponseTemplate::new(412).set_body_string("Invalid query hash")
    }

    /// A fixture whose registry holds a stale searchDesktop hash.
    async fn stale_fixture(server: &MockServer) -> Fixture {
        let f = fixture(server, Duration::ZERO).await;
        f.web
            .registry()
            .update(BTreeMap::from([("searchDesktop".to_string(), stale())]), 0)
            .await
            .unwrap();
        f
    }

    async fn requests_to(server: &MockServer, at: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == at)
            .count()
    }

    async fn cached(f: &Fixture) -> Value {
        serde_json::from_slice(&std::fs::read(f.registry_path()).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn query_sends_persisted_query() {
        let server = MockServer::start().await;
        let reply = json!({"data": {"home": {"x": 1}}, "extensions": {}});
        Mock::given(method("POST"))
            .and(path(QUERY_PATH))
            .and(header("authorization", "Bearer b1"))
            .and(header("client-token", "ct"))
            .and(header("app-platform", "WebPlayer"))
            .and(body_json(json!({
                "operationName": "home",
                "variables": {"timeZone": "UTC"},
                "extensions": {"persistedQuery": {
                    "version": 1,
                    "sha256Hash": Op::Home.fallback_hash(),
                }},
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(&reply))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(&server, Duration::ZERO).await;

        let got = f
            .web
            .query(Lane::User, Op::Home, json!({"timeZone": "UTC"}))
            .await
            .unwrap();
        assert_eq!(got, reply, "the whole body");
        server.verify().await;
    }

    #[tokio::test]
    async fn query_heals_on_412() {
        let server = MockServer::start().await;
        mount_site(&server).await;
        answer(&server, &stale(), invalid_hash()).await;
        let reply = json!({"data": {"searchV2": {}}});
        answer(
            &server,
            SEARCH_DESKTOP,
            ResponseTemplate::new(200).set_body_json(&reply),
        )
        .await;
        let f = stale_fixture(&server).await;

        let got = f
            .web
            .query(Lane::User, Op::SearchDesktop, json!({"searchTerm": "x"}))
            .await
            .unwrap();
        assert_eq!(got, reply);
        assert_eq!(f.web.registry().hash(Op::SearchDesktop), SEARCH_DESKTOP);
        assert_eq!(cached(&f).await["hashes"]["searchDesktop"], SEARCH_DESKTOP);
        assert_eq!(requests_to(&server, QUERY_PATH).await, 2);
        assert_eq!(requests_to(&server, "/").await, 1);
    }

    #[tokio::test]
    async fn graphql_error_surfaces() {
        let server = MockServer::start().await;
        answer(
            &server,
            Op::Home.fallback_hash(),
            ResponseTemplate::new(200)
                .set_body_json(json!({"errors": [{"message": "boom"}], "data": null})),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let err = f
            .web
            .query(Lane::User, Op::Home, json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SpotError::GraphQl(m) if m == "boom"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn graphql_errors_beside_data_are_returned() {
        let server = MockServer::start().await;
        let reply = json!({"errors": [{"message": "partial"}], "data": {"x": 1}});
        answer(
            &server,
            Op::Home.fallback_hash(),
            ResponseTemplate::new(200).set_body_json(&reply),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let got = f.web.query(Lane::User, Op::Home, json!({})).await.unwrap();
        assert_eq!(got, reply);
    }

    #[tokio::test]
    async fn other_status_is_http_error() {
        let server = MockServer::start().await;
        answer(
            &server,
            Op::Home.fallback_hash(),
            ResponseTemplate::new(500),
        )
        .await;
        let f = fixture(&server, Duration::ZERO).await;

        let err = f
            .web
            .query(Lane::User, Op::Home, json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SpotError::Http { status: 500, what } if what == "/pathfinder/v2/query (home)"),
            "{err:?}"
        );
        assert_eq!(requests_to(&server, "/").await, 0, "only a 412 scrapes");
    }

    #[tokio::test]
    async fn failed_scrape_returns_the_412() {
        // The index page names no bundle: the scrape fails and nothing is saved.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>"))
            .mount(&server)
            .await;
        answer(&server, Op::Home.fallback_hash(), invalid_hash()).await;
        let f = fixture(&server, Duration::ZERO).await;

        let err = f
            .web
            .query(Lane::User, Op::Home, json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SpotError::Http { status: 412, what } if what == "/pathfinder/v2/query (home)"),
            "{err:?}"
        );
        assert!(!f.registry_path().exists(), "registry untouched");
        assert!(f.web.registry().is_due(0), "the daily refresh is still due");
        assert_eq!(requests_to(&server, QUERY_PATH).await, 1, "no retry");
    }

    #[tokio::test]
    async fn empty_scrape_returns_the_412() {
        // The bundle loads but holds no op and no chunk: nothing is saved.
        let server = MockServer::start().await;
        let index = format!(
            r#"<script src="{}/cdn/build/web-player/web-player.0123abcd.js"></script>"#,
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(index))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/cdn/build/web-player/web-player.0123abcd.js"))
            .respond_with(ResponseTemplate::new(200).set_body_string("(()=>{})();"))
            .mount(&server)
            .await;
        answer(&server, Op::Home.fallback_hash(), invalid_hash()).await;
        let f = fixture(&server, Duration::ZERO).await;

        let err = f
            .web
            .query(Lane::User, Op::Home, json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SpotError::Http { status: 412, .. }),
            "{err:?}"
        );
        assert!(!f.registry_path().exists(), "registry untouched");
        assert!(f.web.registry().is_due(0));
    }

    #[tokio::test]
    async fn retries_only_once() {
        let server = MockServer::start().await;
        mount_site(&server).await;
        answer(&server, &stale(), invalid_hash()).await;
        answer(&server, SEARCH_DESKTOP, invalid_hash()).await;
        let f = stale_fixture(&server).await;

        let err = f
            .web
            .query(Lane::User, Op::SearchDesktop, json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SpotError::Http { status: 412, .. }),
            "{err:?}"
        );
        assert_eq!(requests_to(&server, QUERY_PATH).await, 2);
        assert_eq!(requests_to(&server, "/").await, 1);
    }

    #[tokio::test]
    async fn concurrent_412s_scrape_once() {
        let server = MockServer::start().await;
        mount_site(&server).await;
        answer(&server, &stale(), invalid_hash()).await;
        answer(
            &server,
            SEARCH_DESKTOP,
            ResponseTemplate::new(200).set_body_json(json!({"data": {}})),
        )
        .await;
        let f = stale_fixture(&server).await;

        // Both queries get their 412 while the lock is held, then queue for it.
        let held = f.web.registry().refresh_lock().lock().await;
        let queries: Vec<_> = (0..2)
            .map(|_| {
                let web = f.web.clone();
                tokio::spawn(
                    async move { web.query(Lane::User, Op::SearchDesktop, json!({})).await },
                )
            })
            .collect();
        tokio::time::timeout(Duration::from_secs(5), async {
            while requests_to(&server, QUERY_PATH).await < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both queries reach the server");
        drop(held);

        for query in queries {
            query.await.unwrap().unwrap();
        }
        assert_eq!(requests_to(&server, "/").await, 1, "one scrape");
        assert_eq!(requests_to(&server, QUERY_PATH).await, 4);
    }

    #[tokio::test]
    async fn refresh_when_due_scrapes_every_op() {
        let server = MockServer::start().await;
        mount_site(&server).await;
        let f = fixture(&server, Duration::ZERO).await;
        let now = 1_000 * HOUR_MS;

        f.web.refresh_hashes_if_due(now).await.unwrap();
        let file = cached(&f).await;
        assert_eq!(file["fetched_at_ms"], now);
        assert_eq!(file["hashes"]["searchDesktop"], SEARCH_DESKTOP);
        assert_eq!(
            file["hashes"]["fetchPlaylist"],
            Op::FetchPlaylist.fallback_hash()
        );
        assert_eq!(
            file["hashes"]["addToLibrary"],
            Op::AddToLibrary.fallback_hash()
        );
        assert!(!f.web.registry().is_due(now + HOUR_MS));
        // Op::ALL is wanted and some ops are in no fixture: every chunk was tried.
        assert_eq!(server.received_requests().await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn refresh_not_due_sends_nothing() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let f = fixture(&server, Duration::ZERO).await;
        f.web.registry().update(BTreeMap::new(), 0).await.unwrap();

        f.web.refresh_hashes_if_due(HOUR_MS).await.unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn forced_refresh_scrapes_when_not_due() {
        let server = MockServer::start().await;
        mount_site(&server).await;
        let f = fixture(&server, Duration::ZERO).await;
        f.web.registry().update(BTreeMap::new(), 0).await.unwrap();
        assert!(!f.web.registry().is_due(HOUR_MS));

        // fetchPlaylist and addToLibrary from the bundle, searchDesktop from its chunk.
        assert_eq!(f.web.refresh_hashes(HOUR_MS).await.unwrap(), 3);
        let file = cached(&f).await;
        assert_eq!(file["fetched_at_ms"], HOUR_MS);
        assert_eq!(file["hashes"]["searchDesktop"], SEARCH_DESKTOP);
        assert!(f.web.registry().is_cached(Op::SearchDesktop));
    }

    #[tokio::test]
    async fn refresh_rechecks_after_the_lock() {
        // A 412 heal saves a scrape while the refresh waits for the lock: the refresh
        // must not scrape again.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let f = fixture(&server, Duration::ZERO).await;
        let now = 1_000 * HOUR_MS;
        assert!(f.web.registry().is_due(now));

        let held = f.web.registry().refresh_lock().lock().await;
        let web = f.web.clone();
        let refresh = tokio::spawn(async move { web.refresh_hashes_if_due(now).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        f.web
            .registry()
            .update(BTreeMap::from([("home".to_string(), "cd".repeat(32))]), now)
            .await
            .unwrap();
        drop(held);

        refresh.await.unwrap().unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn failed_refresh_keeps_it_due() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let f = fixture(&server, Duration::ZERO).await;

        let err = f.web.refresh_hashes_if_due(0).await.unwrap_err();
        assert!(
            matches!(err, SpotError::Http { status: 503, .. }),
            "{err:?}"
        );
        assert!(f.web.registry().is_due(0));
        assert!(!f.registry_path().exists());
        // A reload of the untouched cache still falls back.
        let reloaded = HashRegistry::load(f.registry_path()).await;
        assert_eq!(
            reloaded.hash(Op::SearchDesktop),
            Op::SearchDesktop.fallback_hash()
        );
    }

    /// Moves tokio's clock on. wiremock serves from its own thread and runtime, and no
    /// request is in flight, so only the cool-down sees the jump.
    async fn skip(by: Duration) {
        tokio::time::pause();
        tokio::time::advance(by).await;
        tokio::time::resume();
    }

    /// An index page that names no bundle: every scrape fails after one request.
    async fn mount_broken_site(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>"))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn waiters_after_a_failed_scrape_do_not_scrape() {
        // Two ops, so the cool-down (per op) can't be what stops the second scrape.
        let server = MockServer::start().await;
        mount_broken_site(&server).await;
        answer(&server, &stale(), invalid_hash()).await;
        answer(&server, Op::Home.fallback_hash(), invalid_hash()).await;
        let f = stale_fixture(&server).await;

        let held = f.web.registry().refresh_lock().lock().await;
        let queries: Vec<_> = [Op::SearchDesktop, Op::Home]
            .into_iter()
            .map(|op| {
                let web = f.web.clone();
                tokio::spawn(async move { web.query(Lane::User, op, json!({})).await })
            })
            .collect();
        tokio::time::timeout(Duration::from_secs(5), async {
            while requests_to(&server, QUERY_PATH).await < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both queries reach the server");
        drop(held);

        for query in queries {
            let err = query.await.unwrap().unwrap_err();
            assert!(
                matches!(&err, SpotError::Http { status: 412, .. }),
                "{err:?}"
            );
        }
        assert_eq!(requests_to(&server, "/").await, 1, "one scrape");
        assert_eq!(requests_to(&server, QUERY_PATH).await, 2, "no retries");
    }

    #[tokio::test]
    async fn heal_after_a_refresh_scrape_does_not_scrape() {
        // The daily refresh holds the lock and its scrape fails; the 412 queued behind
        // it doesn't scrape again.
        let server = MockServer::start().await;
        mount_broken_site(&server).await;
        answer(&server, Op::Home.fallback_hash(), invalid_hash()).await;
        let f = fixture(&server, Duration::ZERO).await;

        let held = f.web.registry().refresh_lock().lock().await;
        let web = f.web.clone();
        let refresh = tokio::spawn(async move { web.refresh_hashes_if_due(0).await });
        // The refresh is first in the lock's queue.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let web = f.web.clone();
        let query = tokio::spawn(async move { web.query(Lane::User, Op::Home, json!({})).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while requests_to(&server, QUERY_PATH).await < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the query reaches the server");
        drop(held);

        assert!(refresh.await.unwrap().is_err());
        let err = query.await.unwrap().unwrap_err();
        assert!(
            matches!(&err, SpotError::Http { status: 412, .. }),
            "{err:?}"
        );
        assert_eq!(requests_to(&server, "/").await, 1, "one scrape");
    }

    #[tokio::test]
    async fn unfindable_op_cools_down() {
        // Home is in no fixture: a heal scrape can't renew it.
        let server = MockServer::start().await;
        mount_site(&server).await;
        answer(&server, Op::Home.fallback_hash(), invalid_hash()).await;
        let f = fixture(&server, Duration::ZERO).await;
        let home_412 = || async {
            let err = f
                .web
                .query(Lane::User, Op::Home, json!({}))
                .await
                .unwrap_err();
            assert!(
                matches!(&err, SpotError::Http { status: 412, .. }),
                "{err:?}"
            );
        };
        home_412().await;
        assert_eq!(requests_to(&server, "/").await, 1, "the first 412 scrapes");
        home_412().await;
        assert_eq!(requests_to(&server, "/").await, 1, "cooling down");
        skip(HEAL_COOLDOWN - Duration::from_secs(1)).await;
        home_412().await;
        assert_eq!(requests_to(&server, "/").await, 1, "still cooling down");
        skip(Duration::from_secs(1)).await;
        home_412().await;
        assert_eq!(
            requests_to(&server, "/").await,
            2,
            "cooled down: scrapes again"
        );
        // The fixtures' other ops were saved by the first scrape all the same.
        assert_eq!(cached(&f).await["hashes"]["searchDesktop"], SEARCH_DESKTOP);
    }

    #[tokio::test]
    async fn refresh_skips_after_a_failed_scrape_it_waited_for() {
        // A 412 heal's scrape fails while the refresh waits for the lock: nothing was
        // saved, so the refresh is still due, but it doesn't scrape straight again.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let f = fixture(&server, Duration::ZERO).await;

        let held = f.web.registry().refresh_lock().lock().await;
        let web = f.web.clone();
        let refresh = tokio::spawn(async move { web.refresh_hashes_if_due(0).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        f.web.registry().scrape_finished();
        drop(held);

        refresh.await.unwrap().unwrap();
        assert!(f.web.registry().is_due(0));
        server.verify().await;
    }
}
