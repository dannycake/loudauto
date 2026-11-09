use std::path::PathBuf;

/// Where users get Soloist; the binary is never bundled or redistributed.
pub const DOWNLOADS_URL: &str =
    "https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates";

/// The command install.sh puts on the PATH: it downloads Soloist's newest build from Spotify.
pub const UPDATE_COMMAND: &str = "loudauto-update-soloist";

#[derive(Debug, thiserror::Error)]
pub enum SoloistError {
    #[error(
        "Spotify Soloist not found at {0}; run {UPDATE_COMMAND}, or download it from {DOWNLOADS_URL}"
    )]
    BinaryMissing(PathBuf),
    #[error(
        "this Spotify Soloist build has expired; run {UPDATE_COMMAND}, or install a newer one from {DOWNLOADS_URL}"
    )]
    Expired,
    #[error("Spotify Soloist exited ({code:?}): {tail}")]
    Exited { code: Option<i32>, tail: String },
    #[error(
        "Spotify Soloist is already running (pid {0}); stop that player (the LOUDAUTO engine or `loudauto-cli player`) first"
    )]
    AlreadyRunning(u32),
    #[error("Spotify Soloist did not open its WebSocket in time")]
    StartTimeout,
    #[error("Soloist API key: {0}")]
    Key(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Soloist WebSocket: {0}")]
    WebSocket(String),
    #[error("Soloist connection closed")]
    Closed,
}

pub type Result<T> = std::result::Result<T, SoloistError>;
