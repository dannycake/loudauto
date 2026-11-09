//! The HTTP client for Spotify's web-player endpoints. One `reqwest::Client` carries
//! every request; authenticated ones get the web player's headers, and a 401 fetches
//! the bearer again and retries once. Errors name a URL by its path only: no query
//! string and never a token reaches an error or a log.

use std::{error::Error as _, sync::Arc, time::Duration};

use reqwest::{
    Method, Response, StatusCode,
    header::{self, ACCEPT, AUTHORIZATION, HeaderValue},
};
use serde_json::Value;

use super::{
    USER_AGENT,
    lanes::{Lane, Lanes},
    registry::HashRegistry,
    tokens::Tokens,
};
use crate::{Result, SpotError};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Where each family of endpoints lives; tests point these at a mock server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bases {
    /// The spclient host; paths such as `/color-lyrics/v2/…` are appended.
    pub spclient: String,
    /// The pathfinder GraphQL endpoint (a full URL).
    pub pathfinder: String,
    /// The public web player, whose index page names the current bundle.
    pub public_site: String,
    /// Where the web player's JS chunks are served from.
    pub chunk_base: String,
}

impl Bases {
    pub fn production() -> Self {
        Bases {
            spclient: "https://spclient.wg.spotify.com".into(),
            pathfinder: "https://api-partner.spotify.com/pathfinder/v2/query".into(),
            public_site: "https://open.spotify.com/".into(),
            chunk_base: "https://open.spotifycdn.com/cdn/build/web-player/".into(),
        }
    }
}

#[derive(Clone)]
pub struct Web {
    http: reqwest::Client,
    tokens: Arc<dyn Tokens>,
    lanes: Arc<Lanes>,
    registry: Arc<HashRegistry>,
    bases: Bases,
}

impl Web {
    /// A client for Spotify's production endpoints.
    ///
    /// # Panics
    /// If the TLS backend cannot be initialized, as `reqwest::Client::new` does.
    pub fn new(tokens: Arc<dyn Tokens>, lanes: Arc<Lanes>, registry: Arc<HashRegistry>) -> Web {
        Self::with_bases(tokens, lanes, registry, Bases::production())
    }

    /// A client for the endpoints in `bases` (tests use a mock server).
    ///
    /// # Panics
    /// If the TLS backend cannot be initialized, as `reqwest::Client::new` does.
    pub fn with_bases(
        tokens: Arc<dyn Tokens>,
        lanes: Arc<Lanes>,
        registry: Arc<HashRegistry>,
        bases: Bases,
    ) -> Web {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("TLS backend initializes");
        Web {
            http,
            tokens,
            lanes,
            registry,
            bases,
        }
    }

    pub fn registry(&self) -> &Arc<HashRegistry> {
        &self.registry
    }

    pub fn bases(&self) -> &Bases {
        &self.bases
    }

    /// GET an spclient `path` (query string included) as JSON; 404 is `None`.
    pub async fn spclient_get(&self, lane: Lane, path: &str) -> Result<Option<Value>> {
        let url = format!("{}{path}", self.bases.spclient);
        let resp = self.send_authed(lane, Method::GET, &url, None).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        json_body(success(resp)?).await.map(Some)
    }

    /// POST `body` as JSON to an spclient `path`; an empty reply is `Value::Null`.
    pub async fn spclient_post(&self, lane: Lane, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{path}", self.bases.spclient);
        let resp = self
            .send_authed(lane, Method::POST, &url, Some(body))
            .await?;
        json_body(success(resp)?).await
    }

    /// GET a public page or script: only the user-agent, no credentials.
    pub async fn public_get_text(&self, lane: Lane, url: &str) -> Result<String> {
        self.public_get_text_as(lane, url, USER_AGENT).await
    }

    /// [`Web::public_get_text`] sent as `user_agent` instead of [`USER_AGENT`].
    pub(crate) async fn public_get_text_as(
        &self,
        lane: Lane,
        url: &str,
        user_agent: &'static str,
    ) -> Result<String> {
        if !self.lanes.admit(lane).await {
            return Err(SpotError::Dropped);
        }
        let resp = self
            .http
            .get(url)
            .header(header::USER_AGENT, user_agent)
            .send()
            .await
            .map_err(net)?;
        success(resp)?.text().await.map_err(net)
    }

    /// Sends one authenticated request on `lane` and returns the response whatever its
    /// status, except that a first 401 fetches the bearer again and retries once. A
    /// refused Prefetch is `Dropped` before any token is asked for or request sent.
    pub(crate) async fn send_authed(
        &self,
        lane: Lane,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Response> {
        self.send_authed_as(lane, method, url, body, USER_AGENT)
            .await
    }

    /// [`Web::send_authed`] sent with `user_agent` instead of [`USER_AGENT`].
    pub(crate) async fn send_authed_as(
        &self,
        lane: Lane,
        method: Method,
        url: &str,
        body: Option<&Value>,
        user_agent: &'static str,
    ) -> Result<Response> {
        if !self.lanes.admit(lane).await {
            return Err(SpotError::Dropped);
        }
        let client_token = self.tokens.client_token().await?;
        let bearer = self.tokens.bearer().await?;
        let resp = self
            .send_once(
                method.clone(),
                url,
                &bearer,
                &client_token,
                body,
                user_agent,
            )
            .await?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        log::debug!("401 from {}: fetching the bearer again", resp.url().path());
        let bearer = self.tokens.bearer().await?;
        self.send_once(method, url, &bearer, &client_token, body, user_agent)
            .await
    }

    async fn send_once(
        &self,
        method: Method,
        url: &str,
        bearer: &str,
        client_token: &str,
        body: Option<&Value>,
        user_agent: &'static str,
    ) -> Result<Response> {
        let mut req = self
            .http
            .request(method, url)
            .header(header::USER_AGENT, user_agent)
            .header(AUTHORIZATION, secret_header(&format!("Bearer {bearer}"))?)
            .header("client-token", secret_header(client_token)?)
            .header("app-platform", "WebPlayer")
            .header(ACCEPT, "application/json");
        if let Some(body) = body {
            req = req.json(body);
        }
        req.send().await.map_err(net)
    }
}

/// A header value kept out of `Debug` output.
fn secret_header(value: &str) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| SpotError::Net("a token is not a valid header value".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

/// Any status but 2xx becomes `Http`, named by the URL path alone.
pub(super) fn success(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        Err(SpotError::Http {
            status: status.as_u16(),
            what: resp.url().path().to_string(),
        })
    }
}

/// The body as JSON; an empty body (a bare 200 or 204) is `Value::Null`.
pub(super) async fn json_body(resp: Response) -> Result<Value> {
    let bytes = resp.bytes().await.map_err(net)?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// A transport failure (connect, timeout, a body cut short) as `Net`: the URL path,
/// then reqwest's error chain with the URL stripped, so no query string leaks.
fn net(e: reqwest::Error) -> SpotError {
    let path = e.url().map(|u| u.path().to_string());
    let e = e.without_url();
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        msg.push_str(": ");
        msg.push_str(&cause.to_string());
        source = cause.source();
    }
    SpotError::Net(match path {
        Some(path) => format!("{path}: {msg}"),
        None => msg,
    })
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::Ordering, time::Duration};

    use serde_json::json;
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{body_json, header, method, path, query_param},
    };

    use super::*;
    use crate::web::{lanes::BACKGROUND_GAP, test_support::fixture};

    fn user_agent_is_ours(req: &Request) -> bool {
        req.headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ua| ua.starts_with("LOUDAUTO/"))
    }

    #[test]
    fn user_agent_carries_the_version() {
        assert_eq!(
            USER_AGENT,
            format!("LOUDAUTO/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn production_bases() {
        assert_eq!(
            Bases::production(),
            Bases {
                spclient: "https://spclient.wg.spotify.com".into(),
                pathfinder: "https://api-partner.spotify.com/pathfinder/v2/query".into(),
                public_site: "https://open.spotify.com/".into(),
                chunk_base: "https://open.spotifycdn.com/cdn/build/web-player/".into(),
            }
        );
    }

    /// Matches a request carrying `ua` as its one and only user-agent.
    fn only_user_agent(ua: &'static str) -> impl Fn(&Request) -> bool {
        move |req: &Request| {
            let all: Vec<_> = req.headers.get_all("user-agent").iter().collect();
            all.len() == 1 && all[0] == ua
        }
    }

    #[tokio::test]
    async fn pathfinder_sends_the_browser_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/pathfinder/v2/query"))
            .and(only_user_agent(crate::web::PATHFINDER_USER_AGENT))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let got = f
            .web
            .query(Lane::User, crate::web::ops::Op::Home, json!({}))
            .await
            .unwrap();
        assert_eq!(got, json!({"data": {}}));
    }

    #[tokio::test]
    async fn spclient_sends_the_app_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/color-lyrics/v2/track/x"))
            .and(only_user_agent(USER_AGENT))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/connect-state/v1/x"))
            .and(only_user_agent(USER_AGENT))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let got = f
            .web
            .spclient_get(Lane::User, "/color-lyrics/v2/track/x")
            .await
            .unwrap();
        assert_eq!(got, Some(json!({"ok": true})));
        f.web
            .spclient_post(Lane::User, "/connect-state/v1/x", &json!({}))
            .await
            .unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn sends_required_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/thing"))
            .and(query_param("x", "1"))
            .and(header("authorization", "Bearer b1"))
            .and(header("client-token", "ct"))
            .and(header("app-platform", "WebPlayer"))
            .and(header("accept", "application/json"))
            .and(user_agent_is_ours)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let got = f
            .web
            .spclient_get(Lane::User, "/v1/thing?x=1")
            .await
            .unwrap();
        assert_eq!(got, Some(json!({"ok": true})));
        server.verify().await;
    }

    #[tokio::test]
    async fn retries_once_after_401() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/thing"))
            .and(header("authorization", "Bearer b1"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/thing"))
            .and(header("authorization", "Bearer b2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let got = f.web.spclient_get(Lane::User, "/v1/thing").await.unwrap();
        assert_eq!(got, Some(json!({"ok": true})));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        server.verify().await;
    }

    #[tokio::test]
    async fn second_401_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/thing"))
            .respond_with(ResponseTemplate::new(401))
            .expect(2)
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let err = f
            .web
            .spclient_get(Lane::User, "/v1/thing?secret=s3")
            .await
            .unwrap_err();
        match err {
            SpotError::Http { status, what } => {
                assert_eq!(status, 401);
                // The path only: no query string in an error.
                assert_eq!(what, "/v1/thing");
            }
            other => panic!("expected Http 401, got {other:?}"),
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn not_found_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/missing"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let got = f.web.spclient_get(Lane::User, "/v1/missing").await.unwrap();
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn other_status_is_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/cmd"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let err = f
            .web
            .spclient_post(Lane::User, "/v1/cmd?q=1", &json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SpotError::Http { status: 500, what } if what == "/v1/cmd"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn post_sends_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/cmd"))
            .and(header("content-type", "application/json"))
            .and(header("authorization", "Bearer b1"))
            .and(header("client-token", "ct"))
            .and(header("app-platform", "WebPlayer"))
            .and(user_agent_is_ours)
            .and(body_json(json!({"a": 1})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ack": "x"})))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let got = f
            .web
            .spclient_post(Lane::User, "/v1/cmd", &json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(got, json!({"ack": "x"}));
        server.verify().await;
    }

    #[tokio::test]
    async fn empty_reply_is_null() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/cmd"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let got = f
            .web
            .spclient_post(Lane::User, "/v1/cmd", &json!({}))
            .await
            .unwrap();
        assert_eq!(got, Value::Null);
    }

    #[tokio::test]
    async fn public_get_sends_no_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>"))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let url = f.web.bases().public_site.clone();
        let got = f.web.public_get_text(Lane::User, &url).await.unwrap();
        assert_eq!(got, "<html></html>");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert!(user_agent_is_ours(req));
        for name in ["authorization", "client-token", "app-platform"] {
            assert!(!req.headers.contains_key(name), "{name} was sent");
        }
        assert_eq!(f.tokens.bearer_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn public_get_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let f = fixture(&server, BACKGROUND_GAP).await;

        let url = format!("{}/cdn/build/web-player/x.js?v=2", server.uri());
        let err = f.web.public_get_text(Lane::User, &url).await.unwrap_err();
        assert!(
            matches!(&err, SpotError::Http { status: 503, what } if what == "/cdn/build/web-player/x.js"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn prefetch_refused_is_dropped() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        // A gap no test outlasts: the lane stays busy after the first admission.
        let f = fixture(&server, Duration::from_secs(3600)).await;
        assert!(f.lanes.admit(Lane::Background).await);

        let get = f.web.spclient_get(Lane::Prefetch, "/v1/thing").await;
        assert!(matches!(get, Err(SpotError::Dropped)), "{get:?}");
        let post = f
            .web
            .spclient_post(Lane::Prefetch, "/v1/cmd", &json!({}))
            .await;
        assert!(matches!(post, Err(SpotError::Dropped)), "{post:?}");
        let url = f.web.bases().public_site.clone();
        let public = f.web.public_get_text(Lane::Prefetch, &url).await;
        assert!(matches!(public, Err(SpotError::Dropped)), "{public:?}");

        assert!(server.received_requests().await.unwrap().is_empty());
        assert_eq!(f.tokens.bearer_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unreachable_is_net() {
        // A port that was just free: nothing listens there.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let server = MockServer::start().await;
        let f = fixture(&server, BACKGROUND_GAP).await;
        let web = Web::with_bases(
            f.tokens.clone(),
            f.lanes.clone(),
            f.web.registry().clone(),
            Bases {
                spclient: format!("http://127.0.0.1:{port}"),
                ..f.web.bases().clone()
            },
        );

        let err = web
            .spclient_get(Lane::User, "/v1/thing?x=1")
            .await
            .unwrap_err();
        let SpotError::Net(msg) = &err else {
            panic!("expected Net, got {err:?}");
        };
        assert!(!msg.contains("x=1"), "query string leaked: {msg}");
        assert!(!msg.contains("b1"), "token leaked: {msg}");
    }
}
