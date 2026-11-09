//! Logging in. Always with Spotify's desktop client ID (`SessionConfig::default()`):
//! sessions built on any other client ID fail login5 since 2026-08-10.

use std::{
    path::PathBuf,
    sync::{PoisonError, RwLock},
};

use librespot_core::{
    Error, Session, SessionConfig, authentication::Credentials, cache::Cache, error::ErrorKind,
};
use librespot_oauth::DeviceAuthClientBuilder;

use crate::{Result, SpotError, creds::CredentialStore};

/// What the user must do to approve a new login.
#[derive(Debug, Clone, PartialEq)]
pub struct PairPrompt {
    pub url: String,
    pub code: String,
}

/// The current session, shared by everything that talks to Spotify, so a reconnect
/// swaps the session under every user at once. `Session` is a cheap handle (an `Arc`
/// inside), so `get` hands out a clone instead of holding the lock across a request.
///
/// The engine starts before anyone is logged in, so a cell can also start empty
/// (`empty`): until the first `replace`, `get` answers `NotLoggedIn`.
pub struct SessionCell {
    session: RwLock<Option<Session>>,
}

impl SessionCell {
    pub fn new(session: Session) -> Self {
        Self {
            session: RwLock::new(Some(session)),
        }
    }

    /// A cell with no session yet.
    pub fn empty() -> Self {
        Self {
            session: RwLock::new(None),
        }
    }

    pub fn get(&self) -> Result<Session> {
        // Nothing panics while holding the lock, and a Session is valid either way.
        self.session
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(SpotError::NotLoggedIn)
    }

    pub fn replace(&self, session: Session) {
        *self.session.write().unwrap_or_else(PoisonError::into_inner) = Some(session);
    }
}

/// The scopes Spotify's desktop app requests (librespot @99b6088 `src/main.rs:176`).
const SCOPES: &[&str] = &[
    "app-remote-control",
    "playlist-modify",
    "playlist-modify-private",
    "playlist-modify-public",
    "playlist-read",
    "playlist-read-collaborative",
    "playlist-read-private",
    "streaming",
    "ugc-image-upload",
    "user-follow-modify",
    "user-follow-read",
    "user-library-modify",
    "user-library-read",
    "user-modify",
    "user-modify-playback-state",
    "user-modify-private",
    "user-personalized",
    "user-read-birthdate",
    "user-read-currently-playing",
    "user-read-email",
    "user-read-play-history",
    "user-read-playback-position",
    "user-read-playback-state",
    "user-read-private",
    "user-read-recently-played",
    "user-top-read",
];

/// Spotify said no to these credentials (as opposed to the network failing).
///
/// librespot gives `PermissionDenied` to three different failures; only one of them
/// means the stored login is dead:
/// - a real login rejection (bad credentials, premium required, …) → true
/// - an io EPERM from the socket (VPN kill-switch, firewall OUTPUT drop) → false
/// - `LoginFailed(TryAnotherAP)` after every access point was tried → false
///   (`AuthenticationError` is not public, so this matches its display text,
///   which is fixed in the pinned librespot rev)
pub fn is_auth_rejection(err: &Error) -> bool {
    matches!(err.kind, ErrorKind::PermissionDenied)
        && err.error.downcast_ref::<std::io::Error>().is_none()
        && !err.error.to_string().contains("Try another access point")
}

/// Connect with the stored login. Clears the store only when the stored login is
/// unusable (corrupt, or rejected by Spotify), never on a network failure.
pub async fn connect_stored(store: &dyn CredentialStore) -> Result<Session> {
    let Some(bytes) = store.load().await? else {
        return Err(SpotError::NotLoggedIn);
    };
    let Ok(credentials) = serde_json::from_slice::<Credentials>(&bytes) else {
        store.clear().await?;
        return Err(SpotError::CorruptCredentials);
    };
    let session = new_session(SessionConfig::default())?;
    match session.connect(credentials, false).await {
        Ok(()) => Ok(session),
        Err(e) if is_auth_rejection(&e) => {
            store.clear().await?;
            Err(SpotError::StoredCredentialsRejected)
        }
        Err(e) => Err(e.into()),
    }
}

/// New login through the device-code flow at spotify.com/pair; stores the reusable
/// credentials Spotify hands back.
pub async fn pair(
    store: &dyn CredentialStore,
    on_prompt: impl FnOnce(&PairPrompt) + Send,
) -> Result<Session> {
    let config = SessionConfig::default();
    let client = DeviceAuthClientBuilder::new(&config.client_id, SCOPES.to_vec())
        .build()
        .map_err(pairing_err)?;
    let auth = client
        .request_device_code_async()
        .await
        .map_err(pairing_err)?;
    on_prompt(&PairPrompt {
        url: auth.url().to_string(),
        code: auth.user_code().to_string(),
    });
    let token = client
        .poll_for_token_async(&auth)
        .await
        .map_err(pairing_err)?;

    let session = new_session(config)?;
    session
        .connect(Credentials::with_access_token(token.access_token), true)
        .await?;
    // With store_credentials=true the access point's reusable credentials land in the
    // in-memory cache; those (not the one-shot OAuth token) are what we keep.
    let reusable = session
        .cache()
        .and_then(|cache| cache.credentials())
        .ok_or_else(|| SpotError::Pairing("Spotify returned no reusable credentials".into()))?;
    store.save(&serde_json::to_vec(&reusable)?).await?;
    Ok(session)
}

fn new_session(config: SessionConfig) -> Result<Session> {
    let cache = Cache::new(None::<PathBuf>, None, None, None)?.with_memory_credentials();
    Ok(Session::new(config, Some(cache)))
}

fn pairing_err(e: impl std::fmt::Display) -> SpotError {
    SpotError::Pairing(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::FileStore;

    fn store_in(dir: &tempfile::TempDir) -> FileStore {
        FileStore {
            path: dir.path().join("creds.json"),
        }
    }

    #[tokio::test]
    async fn empty_store_is_not_logged_in() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert!(matches!(
            connect_stored(&store).await,
            Err(SpotError::NotLoggedIn)
        ));
    }

    #[tokio::test]
    async fn corrupt_json_clears_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        std::fs::write(&store.path, b"{not json").unwrap();
        assert!(matches!(
            connect_stored(&store).await,
            Err(SpotError::CorruptCredentials)
        ));
        assert_eq!(store.load().await.unwrap(), None);
    }

    #[test]
    fn socket_eperm_is_not_rejection() {
        // A VPN kill-switch or firewall OUTPUT drop surfaces as io EPERM, which librespot
        // also maps to ErrorKind::PermissionDenied. It must not wipe the stored login.
        let eperm = Error::from(std::io::Error::from_raw_os_error(1));
        assert_eq!(eperm.kind, ErrorKind::PermissionDenied);
        assert!(!is_auth_rejection(&eperm));
    }

    #[test]
    fn try_another_ap_is_not_rejection() {
        // librespot @99b6088 AuthenticationError::LoginFailed(TryAnotherAP) display text.
        let e = Error::permission_denied("Login failed with reason: Try another access point");
        assert!(!is_auth_rejection(&e));
    }

    #[test]
    fn bad_credentials_is_rejection() {
        let e = Error::permission_denied("Login failed with reason: Bad credentials");
        assert!(is_auth_rejection(&e));
    }

    #[tokio::test]
    async fn replace_swaps_the_session_for_every_holder() {
        let first = new_session(SessionConfig::default()).unwrap();
        let second = new_session(SessionConfig::default()).unwrap();
        let (first_id, second_id) = (first.session_id(), second.session_id());
        assert_ne!(first_id, second_id);

        let cell = std::sync::Arc::new(SessionCell::new(first));
        let holder = std::sync::Arc::clone(&cell);
        assert_eq!(holder.get().unwrap().session_id(), first_id);
        cell.replace(second);
        assert_eq!(holder.get().unwrap().session_id(), second_id);
    }

    #[tokio::test]
    async fn empty_cell_is_not_logged_in_until_replaced() {
        let cell = SessionCell::empty();
        assert!(matches!(cell.get(), Err(SpotError::NotLoggedIn)));

        let session = new_session(SessionConfig::default()).unwrap();
        let id = session.session_id();
        cell.replace(session);
        assert_eq!(cell.get().unwrap().session_id(), id);
    }

    #[test]
    fn rejection_vs_transport() {
        assert!(is_auth_rejection(&Error::permission_denied("login failed")));
        assert!(!is_auth_rejection(&Error::unavailable(
            "transport returned no data"
        )));
    }
}
