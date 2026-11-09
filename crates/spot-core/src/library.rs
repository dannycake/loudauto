//! Read-only access to the user's library over a librespot session. Every request
//! waits for its lane first (see `web::lanes`).

use std::sync::Arc;

use librespot_core::{Session, SpotifyUri};
use librespot_metadata::{Metadata, Playlist, Track};

use crate::{
    Result, SpotError,
    context::{collect_entries, entries_from_context},
    cover::largest_cover,
    model::{ItemKind, LibraryEntry, PlaylistDetail, RootEntry, TrackInfo, kind_of},
    rootlist::parse_rootlist,
    search::search_context_uri,
    session::SessionCell,
    web::lanes::{Lane, Lanes},
};

pub struct Library {
    cell: Arc<SessionCell>,
    lanes: Arc<Lanes>,
}

impl Library {
    pub fn new(cell: Arc<SessionCell>, lanes: Arc<Lanes>) -> Self {
        Self { cell, lanes }
    }

    pub fn username(&self) -> Result<String> {
        Ok(self.cell.get()?.username())
    }

    /// Waits for `lane`, then hands out the current session (taken after the wait, so
    /// a reconnect during a Background wait is picked up). `Dropped` for a refused
    /// Prefetch, `NotLoggedIn` while the cell is still empty.
    async fn admit(&self, lane: Lane) -> Result<Session> {
        if self.lanes.admit(lane).await {
            self.cell.get()
        } else {
            Err(SpotError::Dropped)
        }
    }

    pub async fn rootlist(&self, lane: Lane) -> Result<Vec<RootEntry>> {
        let session = self.admit(lane).await?;
        let bytes = session.spclient().get_rootlist(0, Some(500)).await?;
        parse_rootlist(&bytes)
    }

    pub async fn playlist(&self, lane: Lane, uri: &str) -> Result<PlaylistDetail> {
        let id = match SpotifyUri::from_uri(uri) {
            Ok(id @ SpotifyUri::Playlist { .. }) => id,
            _ => return Err(SpotError::NotAPlaylist(uri.to_string())),
        };
        let session = self.admit(lane).await?;
        let playlist = Playlist::get(&session, &id).await?;
        Ok(PlaylistDetail {
            uri: uri.to_string(),
            name: playlist.name().to_string(),
            items: playlist.tracks().map(|t| t.to_uri()).collect(),
        })
    }

    pub async fn liked(&self, lane: Lane) -> Result<Vec<LibraryEntry>> {
        let session = self.admit(lane).await?;
        let uri = format!("spotify:user:{}:collection", session.username());
        let ctx = session.spclient().get_context(&uri).await?;
        // The owner's 758 Liked Songs arrived in one response; bigger libraries may page.
        // An admitted Prefetch finishes its pages as Background rather than being
        // dropped halfway.
        let next_lane = match lane {
            Lane::Prefetch => Lane::Background,
            other => other,
        };
        collect_entries(&ctx, |url| async move {
            let session = self.admit(next_lane).await?;
            Ok(session.spclient().get_next_page(&url).await?.to_vec())
        })
        .await
    }

    pub async fn search(&self, lane: Lane, query: &str) -> Result<Vec<LibraryEntry>> {
        let uri = search_context_uri(query)?;
        let session = self.admit(lane).await?;
        let ctx = session.spclient().get_context(&uri).await?;
        Ok(entries_from_context(&ctx))
    }

    /// Metadata for a track. `Ok(None)` for episodes, local files and anything that
    /// is not a Spotify track — no request is made (and no lane is used) for those.
    pub async fn track(&self, lane: Lane, uri: &str) -> Result<Option<TrackInfo>> {
        if kind_of(uri) != ItemKind::Track {
            return Ok(None);
        }
        let id = SpotifyUri::from_uri(uri)?;
        let session = self.admit(lane).await?;
        let track = Track::get(&session, &id).await?;
        Ok(Some(TrackInfo {
            uri: uri.to_string(),
            title: track.name.clone(),
            artists: track.artists.iter().map(|a| a.name.clone()).collect(),
            album: track.album.name.clone(),
            duration_ms: track.duration.max(0) as u32,
            cover_url: largest_cover(
                track
                    .album
                    .covers
                    .iter()
                    .map(|c| (c.width, c.id.to_base16())),
            ),
        }))
    }
}
