//! Session hints from failed requests: a request that fails on the session tells the
//! session supervisor, which decides whether to reconnect (`SessionHint`). A 401 that
//! survived Web's own token retry needs a new session; a librespot error may mean the
//! session died.

use std::sync::Arc;

use async_trait::async_trait;
use engine_ipc::proto::{Page, PageRequest};
use log::debug;
use spot_core::{
    SpotError,
    web::{ConnectPlay, Web, lanes::Lane},
};
use tokio::sync::mpsc;

use crate::{heart::Liker, pages::PageSource, session::SessionHint, soloist::ConnectPlayer};

/// What a failed request tells the session supervisor, if anything: a 401 that survived
/// Web's own retry needs a new session; a librespot error may mean the session died.
pub fn hint_for(e: &SpotError) -> Option<SessionHint> {
    match e {
        SpotError::Http { status: 401, .. } => Some(SessionHint::AuthRejected),
        SpotError::Librespot(_) => Some(SessionHint::RequestFailed),
        _ => None,
    }
}

pub(crate) fn send_hint(hints: &mpsc::Sender<SessionHint>, e: &SpotError) {
    if let Some(hint) = hint_for(e) {
        debug!("session hint {hint:?} after: {e}");
        // A full channel already holds a hint; one is enough.
        let _ = hints.try_send(hint);
    }
}

/// Wraps a Spotify client so its failures hint the session supervisor (a 401 that
/// survived a token refresh, a librespot error). For the heart, the Connect play
/// command and the pages.
pub struct Hinting<T> {
    inner: T,
    hints: mpsc::Sender<SessionHint>,
}

impl<T> Hinting<T> {
    pub fn new(inner: T, hints: mpsc::Sender<SessionHint>) -> Self {
        Hinting { inner, hints }
    }

    fn check<R>(&self, result: spot_core::Result<R>) -> spot_core::Result<R> {
        if let Err(e) = &result {
            send_hint(&self.hints, e);
        }
        result
    }
}

#[async_trait]
impl Liker for Hinting<Arc<Web>> {
    async fn is_liked(&self, uri: &str) -> spot_core::Result<bool> {
        self.check(Liker::is_liked(&*self.inner, uri).await)
    }

    async fn set_liked(&self, uri: &str, liked: bool) -> spot_core::Result<()> {
        self.check(Liker::set_liked(&*self.inner, uri, liked).await)
    }
}

#[async_trait]
impl ConnectPlayer for Hinting<Arc<Web>> {
    async fn connect_play(&self, from: &str, to: &str, p: &ConnectPlay) -> spot_core::Result<()> {
        self.check(self.inner.connect_play(from, to, p).await)
    }
}

#[async_trait]
impl<S: PageSource> PageSource for Hinting<S> {
    async fn fetch(&self, lane: Lane, req: &PageRequest) -> spot_core::Result<Page> {
        self.check(self.inner.fetch(lane, req).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_for_session_failures_only() {
        let http = |status| SpotError::Http {
            status,
            what: "/pathfinder/v2/query".into(),
        };
        assert_eq!(hint_for(&http(401)), Some(SessionHint::AuthRejected));
        assert_eq!(hint_for(&http(403)), None);
        assert_eq!(hint_for(&http(500)), None);
        let librespot = SpotError::Librespot(librespot_core::Error::unavailable("gone"));
        assert_eq!(hint_for(&librespot), Some(SessionHint::RequestFailed));
        assert_eq!(hint_for(&SpotError::Net("timed out".into())), None);
        assert_eq!(hint_for(&SpotError::NotLoggedIn), None);
    }

    #[tokio::test]
    async fn hinting_wrapper_hints_a_401() {
        let (hints_tx, mut hints) = mpsc::channel(4);
        let pages = Hinting::new(Rejecting, hints_tx);
        assert!(pages.fetch(Lane::User, &PageRequest::Home).await.is_err());
        assert_eq!(hints.try_recv(), Ok(SessionHint::AuthRejected));
    }

    /// Answers every page with a 401.
    struct Rejecting;

    #[async_trait]
    impl PageSource for Rejecting {
        async fn fetch(&self, _: Lane, _: &PageRequest) -> spot_core::Result<Page> {
            Err(SpotError::Http {
                status: 401,
                what: "/pathfinder/v2/query".into(),
            })
        }
    }
}
