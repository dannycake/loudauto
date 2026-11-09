//! MPRIS, `org.mpris.MediaPlayer2.loudauto`: what Plasma's media keys, media widget and
//! lock screen show and control. It follows the engine's player state, so it works with
//! the window closed, and while another device such as the phone plays it shows and
//! controls that device (the engine is a remote then).
//!
//! - It runs on its own thread, on a current-thread runtime and a `LocalSet`: the server
//!   is `!Send`.
//! - Each call becomes an engine `Request`, answered on the same path as a client's.
//! - Properties are read from the latest state when asked. So `PlaybackStatus` is always
//!   current (Plasma's media key sends `Play` or `Pause` by it, not `PlayPause`), and
//!   `Position` is extrapolated to the moment it is read.
//! - Each state change emits `PropertiesChanged` for what differs, and `Seeked` on a
//!   track change or a jump ([`seeked`]).

use std::thread::JoinHandle;

use engine_ipc::{
    APP_ID,
    proto::{PlayerState, Request},
};
use log::{debug, info, warn};
use mpris_server::{
    LocalPlayerInterface, LocalRootInterface, LocalServer, LoopStatus, Metadata, PlaybackRate,
    PlaybackStatus, Property, Signal, Time, TrackId, Volume,
    zbus::{self, fdo},
};
use playback::{
    command::Repeat,
    event::{Actions, PlayStatus},
};
use tokio::{
    sync::{mpsc, watch},
    task::LocalSet,
};

use crate::engine::now_ms;

/// The bus name is `org.mpris.MediaPlayer2.loudauto`.
pub const BUS_NAME_SUFFIX: &str = "loudauto";
pub const IDENTITY: &str = "LOUDAUTO";
/// Where `mpris:trackid`s live: under our own name, as the spec reserves `/org/mpris`.
pub const TRACK_PATH: &str = "/to/skunk/loudauto/track/";
/// How far the reported position may be off the extrapolated one before it is a jump.
pub const JUMP_MS: u64 = 1500;
/// The answer to a call once the engine has stopped taking requests.
pub const ENGINE_GONE: &str = "LOUDAUTO's engine has stopped.";

/// Serves MPRIS on its own thread until the engine drops `state`'s sender. `cmds` gets
/// each call as an engine request. Without a session bus it logs why and ends.
pub fn spawn_mpris(
    state: watch::Receiver<PlayerState>,
    cmds: mpsc::Sender<Request>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("mpris".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    warn!("no MPRIS (media keys, Plasma's widget): no runtime: {e}");
                    return;
                }
            };
            LocalSet::new().block_on(&runtime, async move {
                match serve(state, cmds).await {
                    Ok(()) => debug!("MPRIS stopped: the engine is gone"),
                    Err(e) => warn!("no MPRIS (media keys, Plasma's widget): {e}"),
                }
            });
        })
        .expect("the OS refused a thread for MPRIS")
}

/// Registers on the session bus, then announces each state change until the engine is
/// gone.
async fn serve(
    mut state: watch::Receiver<PlayerState>,
    cmds: mpsc::Sender<Request>,
) -> zbus::Result<()> {
    let imp = Mpris {
        state: state.clone(),
        cmds,
    };
    let server = LocalServer::new(BUS_NAME_SUFFIX, imp).await?;
    tokio::task::spawn_local(server.run());
    info!("MPRIS: serving as {}", server.bus_name());
    let mut prev = state.borrow_and_update().clone();
    while state.changed().await.is_ok() {
        let next = state.borrow_and_update().clone();
        let changed = changes(&prev, &next);
        if !changed.is_empty()
            && let Err(e) = server.properties_changed(changed).await
        {
            warn!("MPRIS: announcing a change failed: {e}");
        }
        if let Some(position) = seeked(&prev, &next, now_ms()) {
            let position = Time::from_micros(i64::try_from(position).unwrap_or(i64::MAX));
            if let Err(e) = server.emit(Signal::Seeked { position }).await {
                warn!("MPRIS: announcing a seek failed: {e}");
            }
        }
        prev = next;
    }
    Ok(())
}

/// The `mpris:trackid` of `uri`: `/to/skunk/loudauto/track/<base62 id>`, or the spec's
/// "no track" for a uri without one (a local file, nothing playing).
pub fn track_id(uri: &str) -> String {
    let mut parts = uri.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("spotify"), Some(kind), Some(id), None)
            if !kind.is_empty()
                && !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            format!("{TRACK_PATH}{id}")
        }
        _ => TrackId::NO_TRACK.to_string(),
    }
}

/// `track_id(uri)` as an object path.
fn object_path(uri: &str) -> TrackId {
    TrackId::try_from(track_id(uri)).unwrap_or(TrackId::NO_TRACK)
}

/// What `p` is playing: trackid, title, artists, album, length and cover (the largest
/// Soloist sends). Nothing playing is the "no track" id alone.
pub fn metadata(p: &PlayerState) -> Metadata {
    let Some(track) = &p.track else {
        return Metadata::builder().trackid(TrackId::NO_TRACK).build();
    };
    let mut m = Metadata::builder().trackid(object_path(&track.uri));
    if !track.title.is_empty() {
        m = m.title(&track.title);
    }
    if !track.artists.is_empty() {
        m = m.artist(&track.artists);
    }
    if let Some(album) = &track.album {
        m = m.album(album);
    }
    if let Some(length) = track.duration_ms {
        m = m.length(micros(length));
    }
    if let Some(cover) = &track.cover_url {
        m = m.art_url(cover);
    }
    m.build()
}

/// The `Can*` flags that change: what the player allows now, for the current track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub can_play: bool,
    pub can_pause: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_seek: bool,
}

/// Each flag is whether Soloist lists the action, and false without a track. `CanPlay`
/// and `CanPause` are both "`play` or `pause` listed": the spec makes them the track's,
/// not the moment's (`CanPlay` is true while playing, `CanPause` while paused), while
/// Soloist lists only the one that applies now (`pause`, while playing). playerctl skips
/// a player whose `CanPlay` is false, even for `metadata`.
pub fn caps(a: &Actions, has_track: bool) -> Caps {
    let playable = has_track && (a.play || a.pause);
    Caps {
        can_play: playable,
        can_pause: playable,
        can_go_next: has_track && a.skip_next,
        can_go_previous: has_track && a.skip_prev,
        can_seek: has_track && a.seek,
    }
}

/// The position to announce with `Seeked`, in µs: on a track change, or when `next`'s
/// position is more than [`JUMP_MS`] off where `prev` would be by now. `None` otherwise,
/// and when nothing plays.
pub fn seeked(prev: &PlayerState, next: &PlayerState, now_ms: i64) -> Option<u64> {
    let track = next.track.as_ref()?;
    let position = position_ms(next, now_ms);
    let same_track = prev.track.as_ref().is_some_and(|t| t.uri == track.uri);
    if same_track && position.abs_diff(position_ms(prev, now_ms)) <= JUMP_MS {
        return None;
    }
    Some(position.saturating_mul(1000))
}

/// Where `p` is at `now_ms`, in ms: extrapolated while playing, held otherwise, and
/// never past the end; 0 when unknown.
fn position_ms(p: &PlayerState, now_ms: i64) -> u64 {
    let Some(position) = &p.position else {
        return 0;
    };
    let at = if playback_status(&p.status) == PlaybackStatus::Playing {
        position.at(now_ms)
    } else {
        position.position_ms
    };
    match p.track.as_ref().and_then(|t| t.duration_ms) {
        Some(length) => at.min(length),
        None => at,
    }
}

fn micros(ms: u64) -> Time {
    Time::from_micros(i64::try_from(ms.saturating_mul(1000)).unwrap_or(i64::MAX))
}

/// Buffering counts as playing: it is on its way, and the key should pause it.
fn playback_status(s: &PlayStatus) -> PlaybackStatus {
    match s {
        PlayStatus::Playing | PlayStatus::Buffering => PlaybackStatus::Playing,
        PlayStatus::Paused => PlaybackStatus::Paused,
        PlayStatus::Stopped | PlayStatus::Other(_) => PlaybackStatus::Stopped,
    }
}

fn loop_status(r: Repeat) -> LoopStatus {
    match r {
        Repeat::Off => LoopStatus::None,
        Repeat::Context => LoopStatus::Playlist,
        Repeat::Track => LoopStatus::Track,
    }
}

fn repeat(l: LoopStatus) -> Repeat {
    match l {
        LoopStatus::None => Repeat::Off,
        LoopStatus::Playlist => Repeat::Context,
        LoopStatus::Track => Repeat::Track,
    }
}

/// `PlayPause` by the status, as reported.
pub(crate) fn play_pause(s: &PlayStatus) -> Request {
    match playback_status(s) {
        PlaybackStatus::Playing => Request::Pause,
        PlaybackStatus::Paused | PlaybackStatus::Stopped => Request::Play,
    }
}

/// `Seek(offset)`: relative to where the track is now, clamped at its start; past its
/// end it acts like `Next`, as the spec says.
fn seek_by(p: &PlayerState, offset: Time, now_ms: i64) -> Option<Request> {
    let track = p.track.as_ref()?;
    let now = i64::try_from(position_ms(p, now_ms)).unwrap_or(i64::MAX);
    let target = u64::try_from(now.saturating_add(offset.as_millis())).unwrap_or(0);
    if track.duration_ms.is_some_and(|length| target >= length) {
        return Some(Request::Next);
    }
    Some(Request::Seek {
        position_ms: target,
    })
}

/// `SetPosition(id, position)`: ignored when `id` is one of ours for another track (it
/// is stale) or the position is outside the track. Another id, such as "no track", still
/// seeks: some clients send one.
fn set_position(p: &PlayerState, id: &TrackId, position: Time) -> Option<Request> {
    let track = p.track.as_ref()?;
    if id.as_str().starts_with(TRACK_PATH) && id.as_str() != track_id(&track.uri) {
        return None;
    }
    let position_ms = u64::try_from(position.as_millis()).ok()?;
    if track.duration_ms.is_some_and(|length| position_ms > length) {
        return None;
    }
    Some(Request::Seek { position_ms })
}

fn volume(v: u8) -> Volume {
    f64::from(v.min(100)) / 100.0
}

/// `Volume` 0.0–1.0 is the engine's 0–100, clamped.
fn set_volume(v: Volume) -> Option<Request> {
    if v.is_nan() {
        return None;
    }
    // Clamped to 0–100 first, so the cast is exact.
    let volume = (v.clamp(0.0, 1.0) * 100.0).round() as u8;
    Some(Request::SetVolume { volume })
}

/// The properties whose values differ between `prev` and `next`, with their new values.
/// `Position` isn't one: clients extrapolate it, and `Seeked` corrects them.
fn changes(prev: &PlayerState, next: &PlayerState) -> Vec<Property> {
    let mut changed = Vec::new();
    let status = playback_status(&next.status);
    if playback_status(&prev.status) != status {
        changed.push(Property::PlaybackStatus(status));
    }
    if prev.repeat != next.repeat {
        changed.push(Property::LoopStatus(loop_status(next.repeat)));
    }
    if prev.shuffle != next.shuffle {
        changed.push(Property::Shuffle(next.shuffle));
    }
    let now = metadata(next);
    if metadata(prev) != now {
        changed.push(Property::Metadata(now));
    }
    if prev.volume != next.volume {
        changed.push(Property::Volume(volume(next.volume)));
    }
    let was = caps(&prev.actions, prev.track.is_some());
    let now = caps(&next.actions, next.track.is_some());
    let flag =
        |was: bool, now: bool, property: fn(bool) -> Property| (was != now).then(|| property(now));
    changed.extend(
        [
            flag(was.can_go_next, now.can_go_next, Property::CanGoNext),
            flag(
                was.can_go_previous,
                now.can_go_previous,
                Property::CanGoPrevious,
            ),
            flag(was.can_play, now.can_play, Property::CanPlay),
            flag(was.can_pause, now.can_pause, Property::CanPause),
            flag(was.can_seek, now.can_seek, Property::CanSeek),
        ]
        .into_iter()
        .flatten(),
    );
    changed
}

/// The D-Bus side: reads the latest state, sends each call to the engine.
struct Mpris {
    state: watch::Receiver<PlayerState>,
    cmds: mpsc::Sender<Request>,
}

impl Mpris {
    /// `f` of the state now. The borrow ends before any await, so the engine is never
    /// held up broadcasting.
    fn with<R>(&self, f: impl FnOnce(&PlayerState) -> R) -> R {
        f(&self.state.borrow())
    }

    async fn send(&self, req: Option<Request>) -> fdo::Result<()> {
        let Some(req) = req else {
            return Ok(());
        };
        debug!("MPRIS: {req:?}");
        self.cmds
            .send(req)
            .await
            .map_err(|_| fdo::Error::Failed(ENGINE_GONE.into()))
    }

    /// `send`, for the property setters.
    async fn set(&self, req: Option<Request>) -> zbus::Result<()> {
        self.send(req).await.map_err(zbus::Error::from)
    }

    fn caps(&self) -> Caps {
        self.with(|p| caps(&p.actions, p.track.is_some()))
    }
}

impl LocalRootInterface for Mpris {
    async fn raise(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> zbus::Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok(IDENTITY.into())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok(APP_ID.into())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl LocalPlayerInterface for Mpris {
    async fn next(&self) -> fdo::Result<()> {
        self.send(Some(Request::Next)).await
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.send(Some(Request::Prev)).await
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.send(Some(Request::Pause)).await
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        let req = self.with(|p| play_pause(&p.status));
        self.send(Some(req)).await
    }

    /// Spotify has no stop; pausing is the nearest.
    async fn stop(&self) -> fdo::Result<()> {
        self.send(Some(Request::Pause)).await
    }

    async fn play(&self) -> fdo::Result<()> {
        self.send(Some(Request::Play)).await
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        let req = self.with(|p| seek_by(p, offset, now_ms()));
        self.send(req).await
    }

    async fn set_position(&self, track_id: TrackId, position: Time) -> fdo::Result<()> {
        let req = self.with(|p| set_position(p, &track_id, position));
        self.send(req).await
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("LOUDAUTO opens no uris.".into()))
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(self.with(|p| playback_status(&p.status)))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(self.with(|p| loop_status(p.repeat)))
    }

    async fn set_loop_status(&self, status: LoopStatus) -> zbus::Result<()> {
        self.set(Some(Request::SetRepeat {
            mode: repeat(status),
        }))
        .await
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    /// The rate is fixed at 1.0.
    async fn set_rate(&self, _rate: PlaybackRate) -> zbus::Result<()> {
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.with(|p| p.shuffle))
    }

    async fn set_shuffle(&self, on: bool) -> zbus::Result<()> {
        self.set(Some(Request::SetShuffle { on })).await
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(self.with(metadata))
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.with(|p| volume(p.volume)))
    }

    async fn set_volume(&self, v: Volume) -> zbus::Result<()> {
        self.set(set_volume(v)).await
    }

    async fn position(&self) -> fdo::Result<Time> {
        Ok(self.with(|p| micros(position_ms(p, now_ms()))))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.caps().can_go_next)
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.caps().can_go_previous)
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(self.caps().can_play)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(self.caps().can_pause)
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.caps().can_seek)
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use playback::{
        event::{NowPlaying, PlaybackEvent, Position, normalize},
        wire::parse_incoming,
    };

    use super::*;
    use crate::{
        core::Core,
        engine::testing::{settings, test_engine},
        soloist::SoloistCmd,
    };
    use engine_ipc::proto::Event;

    /// The 640 px cover, the largest Soloist sends.
    const COVER_640: &str = "https://i.scdn.co/image/ab67616d0000b2734ae1c4c5c45aabe565499163";
    const NERVE: &str = "spotify:track:5kCNaSNJ6AebIX5cM991Tk";
    /// When the fixture's position was reported.
    const T0: i64 = 1_793_954_713_190;

    fn track(uri: &str) -> NowPlaying {
        NowPlaying {
            uri: uri.into(),
            title: "nerve".into(),
            artists: vec!["2hollis".into(), "brakence".into()],
            artist_uris: vec!["spotify:artist:a".into(), "spotify:artist:b".into()],
            album: Some("boy".into()),
            album_uri: Some("spotify:album:b".into()),
            duration_ms: Some(230_526),
            cover_url: Some(COVER_640.into()),
        }
    }

    fn at(position_ms: u64, timestamp_ms: i64) -> Option<Position> {
        Some(Position {
            position_ms,
            timestamp_ms,
            speed: 1.0,
        })
    }

    /// One minute into "nerve" at `T0`, playing.
    fn playing() -> PlayerState {
        PlayerState {
            track: Some(track(NERVE)),
            status: PlayStatus::Playing,
            active_here: true,
            context_uri: Some("spotify:playlist:p".into()),
            position: at(60_000, T0),
            volume: 40,
            shuffle: false,
            repeat: Repeat::Off,
            actions: Actions {
                pause: true,
                seek: true,
                skip_next: true,
                skip_prev: true,
                ..Actions::default()
            },
            liked: None,
            quality: None,
        }
    }

    #[test]
    fn track_id_is_object_path() {
        assert_eq!(
            track_id(NERVE),
            "/to/skunk/loudauto/track/5kCNaSNJ6AebIX5cM991Tk"
        );
        assert_eq!(
            track_id("spotify:episode:0Q86acNRm6V9GYx55SXKwf"),
            "/to/skunk/loudauto/track/0Q86acNRm6V9GYx55SXKwf"
        );
        // No base62 id to put in a path: a local file, nothing, a malformed uri.
        for uri in [
            "spotify:local:2hollis:boy:nerve+%28live%29:180",
            "",
            "spotify:track:",
            "spotify:track:a-b",
            "nerve",
        ] {
            assert_eq!(
                track_id(uri),
                "/org/mpris/MediaPlayer2/TrackList/NoTrack",
                "{uri}"
            );
        }
        // Each is a valid D-Bus object path.
        for uri in [NERVE, "spotify:local:a:b:c:1", ""] {
            TrackId::try_from(track_id(uri)).unwrap();
        }
    }

    #[test]
    fn metadata_from_state() {
        let m = metadata(&playing());
        assert_eq!(
            m.trackid(),
            Some(TrackId::try_from("/to/skunk/loudauto/track/5kCNaSNJ6AebIX5cM991Tk").unwrap())
        );
        assert_eq!(m.title(), Some("nerve"));
        assert_eq!(
            m.artist(),
            Some(vec!["2hollis".to_string(), "brakence".to_string()])
        );
        assert_eq!(m.album(), Some("boy"));
        assert_eq!(m.length(), Some(Time::from_micros(230_526_000)));
        assert_eq!(m.art_url().as_deref(), Some(COVER_640));

        // Nothing playing: only the "no track" id.
        let m = metadata(&PlayerState {
            track: None,
            ..playing()
        });
        assert_eq!(m.trackid(), Some(TrackId::NO_TRACK));
        assert_eq!((m.title(), m.length(), m.art_url()), (None, None, None));
    }

    #[test]
    fn caps_follow_actions() {
        // The phone playing: its frame lists `pause` but not `play`. It can still be
        // played (it is playing), as the spec has it.
        let frame = include_str!("../../playback/tests/fixtures/remote_state.json");
        let remote = normalize(parse_incoming(frame).unwrap())
            .into_iter()
            .find_map(|e| match e {
                PlaybackEvent::Actions(a) => Some(a),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            caps(&remote, true),
            Caps {
                can_play: true,
                can_pause: true,
                can_go_next: true,
                can_go_previous: true,
                can_seek: true,
            }
        );
        // Without a track there is nothing to act on.
        assert_eq!(
            caps(&remote, false),
            Caps {
                can_play: false,
                can_pause: false,
                can_go_next: false,
                can_go_previous: false,
                can_seek: false,
            }
        );
        // Paused, `play` listed: it can still be paused once playing.
        let paused = Actions {
            play: true,
            ..Actions::default()
        };
        assert!(caps(&paused, true).can_play);
        assert!(caps(&paused, true).can_pause);
        // Neither listed: no control of it.
        let none = caps(&Actions::default(), true);
        assert!(!none.can_play && !none.can_pause && !none.can_go_next && !none.can_seek);
    }

    #[test]
    fn seeked_on_jump_only() {
        let prev = playing();
        // Ten seconds on, the extrapolated position is 70 s.
        let now = T0 + 10_000;
        let drift = PlayerState {
            position: at(70_400, now),
            ..prev.clone()
        };
        assert_eq!(seeked(&prev, &drift, now), None);
        let jump = PlayerState {
            position: at(100_000, now),
            ..prev.clone()
        };
        assert_eq!(seeked(&prev, &jump, now), Some(100_000_000));
        // A new track, at its start.
        let next = PlayerState {
            track: Some(track("spotify:track:0000000000000000000002")),
            position: at(0, now),
            ..prev.clone()
        };
        assert_eq!(seeked(&prev, &next, now), Some(0));
        // A pause where it was, then a change a minute later: paused, the position is
        // held, not extrapolated, so neither is a jump.
        let paused = PlayerState {
            status: PlayStatus::Paused,
            position: at(70_000, now),
            ..prev.clone()
        };
        assert_eq!(seeked(&prev, &paused, now), None);
        let louder = PlayerState {
            volume: 50,
            ..paused.clone()
        };
        assert_eq!(seeked(&paused, &louder, now + 60_000), None);
        // Nothing playing any more: nothing to seek.
        let gone = PlayerState {
            track: None,
            position: None,
            ..prev.clone()
        };
        assert_eq!(seeked(&prev, &gone, now), None);
    }

    #[test]
    fn mpris_calls_become_requests() {
        let p = playing();
        let now = T0 + 10_000;
        // Plasma's key sends Play or Pause by the status; PlayPause follows it too.
        assert_eq!(
            playback_status(&PlayStatus::Playing),
            PlaybackStatus::Playing
        );
        assert_eq!(
            playback_status(&PlayStatus::Buffering),
            PlaybackStatus::Playing
        );
        assert_eq!(playback_status(&PlayStatus::Paused), PlaybackStatus::Paused);
        assert_eq!(
            playback_status(&PlayStatus::Stopped),
            PlaybackStatus::Stopped
        );
        assert_eq!(play_pause(&PlayStatus::Playing), Request::Pause);
        assert_eq!(play_pause(&PlayStatus::Paused), Request::Play);

        // Seek is relative to the extrapolated position, clamped at the start; past the
        // end it acts like Next.
        assert_eq!(
            seek_by(&p, Time::from_secs(15), now),
            Some(Request::Seek {
                position_ms: 85_000
            })
        );
        assert_eq!(
            seek_by(&p, Time::from_secs(-600), now),
            Some(Request::Seek { position_ms: 0 })
        );
        assert_eq!(seek_by(&p, Time::from_secs(600), now), Some(Request::Next));
        let nothing = PlayerState {
            track: None,
            ..playing()
        };
        assert_eq!(seek_by(&nothing, Time::from_secs(15), now), None);

        // SetPosition: absolute, for the current track only, within it.
        let ours = TrackId::try_from(track_id(NERVE)).unwrap();
        let stale = TrackId::try_from(track_id("spotify:track:0000000000000000000002")).unwrap();
        assert_eq!(
            set_position(&p, &ours, Time::from_millis(42_000)),
            Some(Request::Seek {
                position_ms: 42_000
            })
        );
        assert_eq!(set_position(&p, &stale, Time::from_millis(42_000)), None);
        assert_eq!(set_position(&p, &ours, Time::from_millis(-1)), None);
        assert_eq!(set_position(&p, &ours, Time::from_secs(231)), None);

        // Volume 0.0–1.0 is 0–100.
        assert_eq!(volume(40), 0.4);
        assert_eq!(set_volume(0.4), Some(Request::SetVolume { volume: 40 }));
        assert_eq!(set_volume(1.7), Some(Request::SetVolume { volume: 100 }));
        assert_eq!(set_volume(-0.2), Some(Request::SetVolume { volume: 0 }));
        assert_eq!(set_volume(f64::NAN), None);

        for (r, l) in [
            (Repeat::Off, LoopStatus::None),
            (Repeat::Context, LoopStatus::Playlist),
            (Repeat::Track, LoopStatus::Track),
        ] {
            assert_eq!(loop_status(r), l);
            assert_eq!(repeat(l), r);
        }
    }

    #[test]
    fn mpris_changes_are_announced() {
        let p = playing();
        assert!(changes(&p, &p.clone()).is_empty());
        // Only a position moved: clients extrapolate it, so no property changed.
        let synced = PlayerState {
            position: at(61_000, T0 + 1_000),
            ..p.clone()
        };
        assert!(changes(&p, &synced).is_empty());
        // Paused at the last track: `play` instead of `pause`, and no next.
        let paused = PlayerState {
            status: PlayStatus::Paused,
            actions: Actions {
                play: true,
                pause: false,
                skip_next: false,
                ..p.actions
            },
            ..p.clone()
        };
        assert_eq!(
            changes(&p, &paused),
            [
                Property::PlaybackStatus(PlaybackStatus::Paused),
                Property::CanGoNext(false),
            ]
        );
        let next = PlayerState {
            track: Some(track("spotify:track:0000000000000000000002")),
            volume: 50,
            ..p.clone()
        };
        assert_eq!(
            changes(&p, &next),
            [Property::Metadata(metadata(&next)), Property::Volume(0.5),]
        );
    }

    #[tokio::test]
    async fn mpris_follows_player_broadcasts() {
        let t = test_engine(Core::new(settings())).await;
        let mut state = t.engine.player_state();
        assert_eq!(state.borrow_and_update().track, None);
        t.engine.broadcast(Event::Player(playing()));
        tokio::time::timeout(Duration::from_secs(5), state.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*state.borrow(), playing());
    }

    #[tokio::test]
    async fn mpris_requests_take_the_clients_path() {
        let mut t = test_engine(Core::new(settings())).await;
        let (requests, rx) = tokio::sync::mpsc::channel(4);
        tokio::spawn(t.engine.clone().on_local_requests(rx));
        requests.send(Request::Pause).await.unwrap();
        let (cmd, reply) = tokio::time::timeout(Duration::from_secs(5), t.soloist.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cmd, SoloistCmd::Pause);
        reply.send(Ok(())).unwrap();
    }
}
