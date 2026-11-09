//! What a failed Spotify request tells the owner: a few words, never the error's own text,
//! which may carry a request's path or a server's message. The journal keeps the whole
//! error.

use spot_core::SpotError;

/// A short reason for the owner, the end of `Couldn't <what>: <reason>`.
pub fn failure_reason(e: &SpotError) -> String {
    use SpotError as E;
    let reason = match e {
        E::Http { status, .. } => return format!("Spotify answered {status}"),
        E::Net(_) => "no connection to Spotify",
        E::Librespot(_) => "the Spotify session failed",
        E::NotLoggedIn | E::StoredCredentialsRejected | E::CorruptCredentials => {
            "not logged in to Spotify"
        }
        E::InvalidUri(_) => "Spotify doesn't know that item",
        E::GraphQl(_) => "Spotify refused the request",
        E::Json(_) | E::Protobuf(_) | E::UnexpectedPage(_) => "Spotify's answer didn't make sense",
        E::Scrape(_) => "couldn't read Spotify's web player",
        E::Io(_)
        | E::Keyring(_)
        | E::EmptyQuery
        | E::Pairing(_)
        | E::NotAPlaylist(_)
        | E::Dropped => "something went wrong",
    };
    reason.to_string()
}
