//! Live tests against the owner's real Spotify account. They pass trivially unless
//! `LOUDAUTO_LIVE=1`, and need a prior `loudauto-cli login`. Run them one at a
//! time so they don't open parallel logins:
//!
//!     LOUDAUTO_LIVE=1 cargo test -p spot-core --test live -- --test-threads=1

use std::sync::Arc;

use spot_core::{
    creds::SecretServiceStore,
    library::Library,
    model::RootEntry,
    session::{SessionCell, connect_stored},
    web::lanes::{BACKGROUND_GAP, Lane, Lanes},
};

fn live() -> bool {
    std::env::var("LOUDAUTO_LIVE").as_deref() == Ok("1")
}

async fn library() -> Library {
    let session = connect_stored(&SecretServiceStore::SPOTIFY_SESSION)
        .await
        .expect("run `loudauto-cli login` first");
    Library::new(
        Arc::new(SessionCell::new(session)),
        Arc::new(Lanes::new(BACKGROUND_GAP)),
    )
}

#[tokio::test]
async fn live_rootlist_has_playlists() {
    if !live() {
        return;
    }
    let roots = library().await.rootlist(Lane::User).await.unwrap();
    assert!(
        roots
            .iter()
            .any(|r| matches!(r, RootEntry::Playlist(p) if !p.name.is_empty())),
        "{roots:?}"
    );
}

#[tokio::test]
async fn live_playlist_has_items() {
    if !live() {
        return;
    }
    let lib = library().await;
    let first = lib
        .rootlist(Lane::User)
        .await
        .unwrap()
        .into_iter()
        .find_map(|r| match r {
            RootEntry::Playlist(p) => Some(p),
            _ => None,
        })
        .expect("at least one playlist");
    let detail = lib.playlist(Lane::User, &first.uri).await.unwrap();
    assert!(!detail.name.is_empty());
    assert!(!detail.items.is_empty());
}

#[tokio::test]
async fn live_liked_has_dates() {
    if !live() {
        return;
    }
    let liked = library().await.liked(Lane::User).await.unwrap();
    assert!(!liked.is_empty());
    let dated = liked
        .iter()
        .filter(|e| e.added_at_ms.is_some_and(|t| t > 1_200_000_000_000))
        .count();
    assert!(
        dated * 10 >= liked.len() * 9,
        "{dated} of {} dated",
        liked.len()
    );
}

#[tokio::test]
async fn live_search_finds_acdc() {
    if !live() {
        return;
    }
    let lib = library().await;
    let hits = lib.search(Lane::User, "AC/DC").await.unwrap();
    assert!(!hits.is_empty());
    let first = lib
        .track(Lane::User, &hits[0].uri)
        .await
        .unwrap()
        .expect("first hit is a track");
    assert!(first.artists.iter().any(|a| a == "AC/DC"), "{first:?}");
}

#[tokio::test]
async fn live_track_skips_local() {
    if !live() {
        return;
    }
    let lib = library().await;
    assert_eq!(
        lib.track(Lane::User, "spotify:local:a:b:c:180")
            .await
            .unwrap(),
        None
    );
}
