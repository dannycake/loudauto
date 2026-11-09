//! Live test of the web data layer against the owner's real Spotify account. It
//! passes trivially unless `LOUDAUTO_LIVE=1`, and needs a prior `loudauto-cli login`.
//! It only reads: no heart, no Connect play.
//!
//!     LOUDAUTO_LIVE=1 cargo test -p spot-core --test web_live -- --nocapture

use std::sync::Arc;

use spot_core::{
    creds::SecretServiceStore,
    session::{SessionCell, connect_stored},
    web::{
        SessionTokens, Web,
        lanes::{BACKGROUND_GAP, Lane, Lanes},
        registry::HashRegistry,
    },
};

const JANE_REMOVER: &str = "spotify:artist:2rLGlNI6htigNxx172qxLu";
const REVENGESEEKERZ: &str = "spotify:album:21b4cDNse2AMpj94ykfuON";
const PLAYLIST: &str = "spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi";
const TRACK: &str = "spotify:track:1XD4K4CGAKTIBmFpvuaFru";

fn live() -> bool {
    std::env::var("LOUDAUTO_LIVE").as_deref() == Ok("1")
}

/// A `Web` on the stored session and the app's own hash cache, as the engine builds it.
async fn web() -> Web {
    let session = connect_stored(&SecretServiceStore::SPOTIFY_SESSION)
        .await
        .expect("run `loudauto-cli login` first");
    let cell = Arc::new(SessionCell::new(session));
    Web::new(
        Arc::new(SessionTokens(cell)),
        Arc::new(Lanes::new(BACKGROUND_GAP)),
        Arc::new(HashRegistry::load(HashRegistry::default_path()).await),
    )
}

/// A step's outcome: `Ok(summary)`, or `Err` with the error or the failed check.
type Step = Result<String, String>;

/// Judges a fetch: `check` gives whether it passed and a one-line summary.
fn step<T>(got: spot_core::Result<T>, check: impl FnOnce(T) -> (bool, String)) -> Step {
    match got {
        Ok(value) => match check(value) {
            (true, summary) => Ok(summary),
            (false, summary) => Err(format!("check failed: {summary}")),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// Runs every step, whatever the earlier ones did, prints one line per step, then
/// fails listing the steps that failed.
#[tokio::test]
async fn live_pages() {
    if !live() {
        return;
    }
    let web = web().await;
    let lane = Lane::User;

    let steps: Vec<(&str, Step)> = vec![
        (
            "home",
            step(web.home(lane).await, |home| {
                let n = home.sections.len();
                (n > 0, format!("{n} sections"))
            }),
        ),
        (
            "search",
            step(web.search(lane, "jane remover").await, |s| {
                (
                    !s.tracks.is_empty(),
                    format!(
                        "{} tracks, {} artists, {} albums, {} playlists",
                        s.tracks.len(),
                        s.artists.len(),
                        s.albums.len(),
                        s.playlists.len()
                    ),
                )
            }),
        ),
        (
            "artist",
            step(web.artist(lane, JANE_REMOVER).await, |a| {
                (
                    a.name == "Jane Remover",
                    format!("{}, {} top tracks", a.name, a.top_tracks.len()),
                )
            }),
        ),
        (
            "album",
            step(web.album(lane, REVENGESEEKERZ).await, |a| {
                (
                    a.tracks.len() == 12,
                    format!("{}, {} rows", a.name, a.tracks.len()),
                )
            }),
        ),
        (
            "playlist",
            step(web.playlist(lane, PLAYLIST).await, |p| {
                (
                    usize::try_from(p.total) == Ok(p.tracks.len()),
                    format!("{} rows of {}", p.tracks.len(), p.total),
                )
            }),
        ),
        (
            "liked",
            step(web.liked_page(lane, 0).await, |l| {
                (
                    l.total >= 700,
                    format!("{} rows of {}", l.tracks.len(), l.total),
                )
            }),
        ),
        (
            "recents",
            step(web.recents(lane).await, |r| {
                (!r.is_empty(), format!("{} cards", r.len()))
            }),
        ),
        (
            "lyrics",
            step(web.lyrics(lane, TRACK).await, |l| match l {
                Some(l) => (
                    l.synced,
                    format!(
                        "{} lines, synced={}, {}",
                        l.lines.len(),
                        l.synced,
                        l.provider
                    ),
                ),
                None => (false, "no lyrics".into()),
            }),
        ),
        (
            "credits",
            step(web.credits(lane, TRACK).await, |c| {
                (!c.is_empty(), format!("{} roles", c.len()))
            }),
        ),
        (
            "radio",
            step(web.radio_for(lane, TRACK).await, |uri| {
                (uri.starts_with("spotify:playlist:"), uri)
            }),
        ),
    ];

    for (name, outcome) in &steps {
        match outcome {
            Ok(summary) => eprintln!("PASS {name}: {summary}"),
            Err(why) => eprintln!("FAIL {name}: {why}"),
        }
    }
    let failed: Vec<&str> = steps
        .iter()
        .filter(|(_, outcome)| outcome.is_err())
        .map(|(name, _)| *name)
        .collect();
    assert!(failed.is_empty(), "failed steps: {failed:?}");
}
