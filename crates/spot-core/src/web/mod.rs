//! Spotify's web-player data layer: the endpoints the web player itself uses.

pub mod client;
pub mod connect;
mod fetch;
pub mod lanes;
pub mod ops;
pub mod pathfinder;
pub mod registry;
pub mod scrape;
#[cfg(test)]
mod test_support;
pub mod tokens;

pub use client::{Bases, Web};
pub use connect::{ConnectPlay, connect_play_body, random_hex};
pub use fetch::LIKED_PAGE_SIZE;
pub use tokens::{SessionTokens, Tokens};

/// The user-agent on every Spotify request (the app's art loader reuses it).
pub const USER_AGENT: &str = concat!("LOUDAUTO/", env!("CARGO_PKG_VERSION"));

/// The user-agent on pathfinder (api-partner.spotify.com) requests only: they answer
/// HTTP 403 to `LOUDAUTO/<ver>`, so they present as the desktop browser Spotify's web
/// player runs in (owner decision, 2026-11-06). Every other request sends
/// [`USER_AGENT`].
pub const PATHFINDER_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// The user-agent on the public index page request only: open.spotify.com serves the
/// mobile player, which has no persisted-query ops, to user-agents it doesn't know.
pub const INDEX_USER_AGENT: &str = concat!(
    "LOUDAUTO/",
    env!("CARGO_PKG_VERSION"),
    " Gecko/20100101 Firefox/140.0"
);
