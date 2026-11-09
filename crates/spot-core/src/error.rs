/// Everything that can go wrong in spot-core.
#[derive(Debug, thiserror::Error)]
pub enum SpotError {
    #[error(transparent)]
    Librespot(#[from] librespot_core::Error),
    #[error("malformed protobuf: {0}")]
    Protobuf(#[from] protobuf::Error),
    #[error("malformed JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("secret service: {0}")]
    Keyring(String),
    #[error("search query is empty")]
    EmptyQuery,
    #[error("not logged in")]
    NotLoggedIn,
    #[error("Spotify rejected the stored login")]
    StoredCredentialsRejected,
    #[error("stored login is corrupt")]
    CorruptCredentials,
    #[error("pairing failed: {0}")]
    Pairing(String),
    /// Spotify answered with something of another shape than expected (a page without
    /// what it should hold, a count that doesn't match): `{0}` names where and what.
    #[error("unexpected answer from Spotify: {0}")]
    UnexpectedPage(String),
    /// A request refused before anything was sent: the uri isn't of a kind it takes
    /// (`{0}` says so, e.g. `spotify:episode:… isn't a track`).
    #[error("{0}")]
    InvalidUri(String),
    #[error("not a playlist URI: {0}")]
    NotAPlaylist(String),
    #[error("prefetch dropped: the background lane is busy")]
    Dropped,
    /// A Spotify endpoint answered with an error status. `what` is the URL path without
    /// its query string, so neither ids nor anything credential-shaped reach a log.
    #[error("HTTP {status} from {what}")]
    Http { status: u16, what: String },
    #[error("GraphQL error: {0}")]
    GraphQl(String),
    /// The request never got an answer: connect failure, timeout, a body cut short.
    #[error("network: {0}")]
    Net(String),
    /// The public web player did not yield persisted-query hashes: no bundle named in
    /// its index page, or none of the wanted ops in its scripts.
    #[error("hash scrape: {0}")]
    Scrape(String),
}

pub type Result<T> = std::result::Result<T, SpotError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Deferred Task 5/7 minor: these reach the window verbatim (a failed page, a stale
    /// line), so they say what happened in general words. A request refused before
    /// anything was sent is its own variant.
    #[test]
    fn unexpected_answers_and_refused_uris_read_plainly() {
        let page = SpotError::UnexpectedPage("getAlbum (no data.albumUnion)".into());
        assert_eq!(
            page.to_string(),
            "unexpected answer from Spotify: getAlbum (no data.albumUnion)"
        );
        let uri = SpotError::InvalidUri("spotify:episode:x isn't a track".into());
        assert_eq!(uri.to_string(), "spotify:episode:x isn't a track");
    }
}
