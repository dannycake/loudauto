//! The engine's state reducer: playback events, liked-set updates and the user's volume go
//! in; the new `EngineState` and the side effects to run come out. Pure, so the rules are
//! tested without Soloist, a socket or the network; the caller executes the effects.

use std::collections::HashSet;

use engine_ipc::proto::{
    EngineState, Event, Level, LoginStep, Notice, PlayerState, SessionHealth, Setting, Settings,
    SoloistHealth,
};
use playback::{
    command::Repeat,
    event::{Actions, PlayStatus, PlaybackEvent},
};

/// The Info notice after the output device changes: Soloist is launched with its device.
pub const OUTPUT_DEVICE_NOTICE: &str = "The new output device is used from LOUDAUTO's next start.";

/// What the caller must do after a state change.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "Broadcast, the large variant, is the common one: boxing it would allocate per \
              broadcast to shrink the rare small ones"
)]
pub enum Effect {
    /// Send to every connected client.
    Broadcast(Event),
    /// Set Soloist's volume (its PipeWire stream volume).
    SetSoloistVolume(u8),
    /// Write `state.settings` to disk.
    SaveSettings,
    /// Ask Spotify whether this uri is liked, then report back with `Core::on_liked`.
    CheckLiked(String),
    /// Show the notice as a desktop notification when no window is open.
    NotifyIfClosed(Notice),
}

/// The engine's state and the liked set behind it. `state.player.liked` always equals
/// `is_liked` of the current track.
pub struct Core {
    pub state: EngineState,
    /// The uris in the owner's Liked Songs: all of them once `filled`; before that, only
    /// those a check or a heart press found liked.
    liked: HashSet<String>,
    /// Whether Liked Songs has been read whole: until then a track's state is unknown,
    /// unless it is in `known`.
    filled: bool,
    /// Before `filled`: the uris whose state a check or a heart press established.
    known: HashSet<String>,
    /// The id of the last notice made.
    last_notice: u64,
}

impl Core {
    /// The state before anything has reported: nothing playing and not the active player,
    /// the persisted volume shown until Soloist reports its own, Soloist starting and the
    /// session connecting. `login` reads `LoggedIn` until the session finds no stored
    /// credentials and says `Needed`, so a client that connects first doesn't flash the
    /// login screen.
    pub fn new(settings: Settings) -> Self {
        let player = PlayerState {
            track: None,
            status: PlayStatus::Stopped,
            active_here: false,
            context_uri: None,
            position: None,
            volume: settings.volume,
            shuffle: false,
            repeat: Repeat::Off,
            actions: Actions::default(),
            liked: None,
            quality: None,
        };
        Core {
            state: EngineState {
                player,
                queue: Vec::new(),
                soloist: SoloistHealth::Starting,
                session: SessionHealth::Connecting,
                login: LoginStep::LoggedIn,
                settings,
                username: None,
                soloist_expires_in_days: None,
            },
            liked: HashSet::new(),
            filled: false,
            known: HashSet::new(),
            last_notice: 0,
        }
    }

    /// Folds one of Soloist's events into the state. A change to the player state is
    /// broadcast once, as the whole `PlayerState`; an event that changes nothing returns
    /// no `Player` broadcast.
    pub fn on_playback(&mut self, ev: &PlaybackEvent) -> Vec<Effect> {
        use PlaybackEvent as E;
        let before = self.state.player.clone();
        let mut effects = Vec::new();
        let player = &mut self.state.player;
        match ev {
            E::Active(active) => {
                // Activation jumps Soloist to 100: put the owner's volume back.
                if *active && !player.active_here {
                    effects.push(Effect::SetSoloistVolume(self.state.settings.volume));
                }
                player.active_here = *active;
            }
            E::Track(track) => {
                // Every playback_state repeats the track, so check only a new one.
                if player.track.as_ref().is_none_or(|t| t.uri != track.uri) {
                    effects.push(Effect::CheckLiked(track.uri.clone()));
                }
                player.liked = is_liked(&self.liked, self.filled, &self.known, &track.uri);
                player.track = Some(track.clone());
            }
            E::Status(status) => player.status = status.clone(),
            E::Position(position) => player.position = Some(*position),
            // Soloist's own report: shown, never persisted (see `on_user_volume`).
            E::Volume(volume) => player.volume = *volume,
            E::Quality(quality) => player.quality = Some(quality.clone()),
            E::Context(uri) => player.context_uri = uri.clone(),
            E::Options { shuffle, repeat } => {
                player.shuffle = *shuffle;
                player.repeat = *repeat;
            }
            E::Actions(actions) => player.actions = *actions,
            E::Upcoming(queue) => {
                if self.state.queue != *queue {
                    self.state.queue = queue.clone();
                    effects.push(Effect::Broadcast(Event::Queue(queue.clone())));
                }
            }
            E::Error(message) => {
                let notice = self.notice(Level::Error, message.clone());
                effects.push(Effect::Broadcast(Event::Notice(notice.clone())));
                effects.push(Effect::NotifyIfClosed(notice));
            }
            E::ExpiresInDays(days) => {
                if self.state.soloist_expires_in_days != Some(*days) {
                    self.state.soloist_expires_in_days = Some(*days);
                    effects.push(Effect::Broadcast(Event::Expiry { days: *days }));
                }
            }
            // The Soloist supervisor owns login and exits (`Engine::on_playback` takes the
            // loop's `Exited` to `on_player_down`).
            E::Auth { .. } | E::Exited { .. } => {}
        }
        if self.state.player != before {
            effects.push(self.player_broadcast());
        }
        effects
    }

    /// Soloist stopped (it crashed, is restarting, or the engine is quitting): nothing
    /// plays here any more, whatever it said last. The player shows paused where it had
    /// got to (`PlayerState::halted`), so MPRIS, the tray and the windows don't say
    /// Playing, or count on, while nothing does.
    pub fn on_player_down(&mut self, now_ms: i64) -> Vec<Effect> {
        let halted = self.state.player.halted(now_ms);
        if halted == self.state.player {
            return vec![];
        }
        self.state.player = halted;
        vec![self.player_broadcast()]
    }

    /// The owner moved the volume: the one path that changes the persisted volume. Clamped
    /// to Soloist's 0–100, so what is saved is what plays. The caller also sends Soloist
    /// the volume; Soloist's report then updates `player.volume`.
    pub fn on_user_volume(&mut self, v: u8) -> Vec<Effect> {
        let v = v.min(100);
        if self.state.settings.volume == v {
            return vec![];
        }
        self.state.settings.volume = v;
        vec![
            Effect::SaveSettings,
            Effect::Broadcast(Event::Settings(self.state.settings.clone())),
        ]
    }

    /// Replaces the liked set with the library's Liked Songs, read whole: the first read,
    /// after which every track's state is known. Re-derives whether the current track is
    /// liked, and reports `LikedSetReady`: what clients were told was unknown is known
    /// now, and what they were told before may have changed.
    pub fn set_liked_set(&mut self, uris: impl IntoIterator<Item = String>) -> Vec<Effect> {
        self.liked = uris.into_iter().collect();
        self.filled = true;
        self.known.clear();
        let mut effects = self.sync_current_liked();
        effects.push(Effect::Broadcast(Event::LikedSetReady));
        effects
    }

    /// Whether Liked Songs has been read whole (`set_liked_set`).
    pub fn liked_filled(&self) -> bool {
        self.filled
    }

    /// Replaces the liked set with a newer copy of the library's: every refresh after
    /// the first (`set_liked_set`). Reports `LikedChanged` for each uri whose state
    /// changed, in uri order, then the player if the current track's did.
    pub fn refresh_liked_set(&mut self, uris: impl IntoIterator<Item = String>) -> Vec<Effect> {
        let fresh: HashSet<String> = uris.into_iter().collect();
        let mut changes: Vec<(String, bool)> = fresh
            .difference(&self.liked)
            .map(|uri| (uri.clone(), true))
            .chain(
                self.liked
                    .difference(&fresh)
                    .map(|uri| (uri.clone(), false)),
            )
            .collect();
        changes.sort();
        self.liked = fresh;
        let mut effects: Vec<Effect> = changes
            .into_iter()
            .map(|(uri, liked)| Effect::Broadcast(Event::LikedChanged { uri, liked }))
            .collect();
        effects.extend(self.sync_current_liked());
        effects
    }

    /// The owner changed a setting: saved and broadcast when it changes anything. A new
    /// output device also says when it takes effect.
    pub fn on_setting(&mut self, setting: Setting) -> Vec<Effect> {
        let settings = &mut self.state.settings;
        let device_changed = match setting {
            Setting::OutputDevice(device) => {
                if settings.output_device == device {
                    return vec![];
                }
                settings.output_device = device;
                true
            }
            Setting::Autostart(on) => {
                if settings.autostart == on {
                    return vec![];
                }
                settings.autostart = on;
                false
            }
        };
        let mut effects = vec![
            Effect::SaveSettings,
            Effect::Broadcast(Event::Settings(self.state.settings.clone())),
        ];
        if device_changed {
            let notice = self.notice(Level::Info, OUTPUT_DEVICE_NOTICE);
            effects.push(Effect::Broadcast(Event::Notice(notice)));
        }
        effects
    }

    /// `uri` was liked or unliked (a heart press, or a `CheckLiked` answer). Always
    /// reports `LikedChanged`, so every client's hearts follow; the player too when `uri`
    /// is the current track.
    pub fn on_liked(&mut self, uri: &str, liked: bool) -> Vec<Effect> {
        if liked {
            self.liked.insert(uri.to_owned());
        } else {
            self.liked.remove(uri);
        }
        if !self.filled {
            self.known.insert(uri.to_owned());
        }
        let mut effects = vec![Effect::Broadcast(Event::LikedChanged {
            uri: uri.to_owned(),
            liked,
        })];
        effects.extend(self.sync_current_liked());
        effects
    }

    /// Whether `uri` is liked; `None` while that isn't known (Liked Songs not read yet,
    /// nor the track checked).
    pub fn is_liked(&self, uri: &str) -> Option<bool> {
        is_liked(&self.liked, self.filled, &self.known, uri)
    }

    /// A new notice, with the next id.
    pub fn notice(&mut self, level: Level, text: impl Into<String>) -> Notice {
        self.last_notice += 1;
        Notice {
            id: self.last_notice,
            level,
            text: text.into(),
        }
    }

    /// Keeps `player.liked` equal to `is_liked` of the current track.
    fn sync_current_liked(&mut self) -> Vec<Effect> {
        let player = &mut self.state.player;
        let liked = player
            .track
            .as_ref()
            .and_then(|t| is_liked(&self.liked, self.filled, &self.known, &t.uri));
        if player.liked == liked {
            return vec![];
        }
        player.liked = liked;
        vec![self.player_broadcast()]
    }

    fn player_broadcast(&self) -> Effect {
        Effect::Broadcast(Event::Player(self.state.player.clone()))
    }
}

/// `Core::is_liked`, on the fields, for where `state` is borrowed mutably.
fn is_liked(
    liked: &HashSet<String>,
    filled: bool,
    known: &HashSet<String>,
    uri: &str,
) -> Option<bool> {
    (filled || known.contains(uri)).then(|| liked.contains(uri))
}

#[cfg(test)]
mod tests {
    use playback::event::{NowPlaying, Position, StreamQuality};

    use super::*;

    fn settings(volume: u8) -> Settings {
        Settings {
            volume,
            output_device: None,
            autostart: false,
        }
    }

    fn core() -> Core {
        Core::new(settings(40))
    }

    fn track(uri: &str) -> NowPlaying {
        NowPlaying {
            uri: uri.into(),
            title: "nerve".into(),
            artists: vec!["2hollis".into()],
            artist_uris: vec!["spotify:artist:a".into()],
            album: Some("boy".into()),
            album_uri: Some("spotify:album:b".into()),
            duration_ms: Some(150_000),
            cover_url: Some("https://i.scdn.co/image/x".into()),
        }
    }

    /// The `Player` broadcasts among `effects`.
    fn players(effects: &[Effect]) -> Vec<&PlayerState> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Broadcast(Event::Player(p)) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn volumes(effects: &[Effect]) -> Vec<u8> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::SetSoloistVolume(v) => Some(*v),
                _ => None,
            })
            .collect()
    }

    fn checks(effects: &[Effect]) -> Vec<&str> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::CheckLiked(uri) => Some(uri.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn activation_restores_volume() {
        let mut core = core();
        let effects = core.on_playback(&PlaybackEvent::Active(true));
        assert_eq!(volumes(&effects), [40]);
        assert!(core.state.player.active_here);
        let again = core.on_playback(&PlaybackEvent::Active(true));
        assert_eq!(volumes(&again), [] as [u8; 0]);
    }

    #[test]
    fn reactivation_restores_volume_again() {
        let mut core = core();
        core.on_playback(&PlaybackEvent::Active(true));
        let off = core.on_playback(&PlaybackEvent::Active(false));
        assert_eq!(volumes(&off), [] as [u8; 0]);
        assert!(!core.state.player.active_here);
        core.on_user_volume(55);
        let on = core.on_playback(&PlaybackEvent::Active(true));
        assert_eq!(volumes(&on), [55]);
    }

    #[test]
    fn soloist_volume_not_persisted() {
        let mut core = core();
        let effects = core.on_playback(&PlaybackEvent::Volume(100));
        assert_eq!(core.state.player.volume, 100);
        assert_eq!(core.state.settings.volume, 40);
        assert!(!effects.contains(&Effect::SaveSettings));
        assert_eq!(players(&effects).len(), 1);
    }

    #[test]
    fn user_volume_persisted() {
        let mut core = core();
        let effects = core.on_user_volume(55);
        assert_eq!(core.state.settings.volume, 55);
        assert_eq!(
            effects,
            [
                Effect::SaveSettings,
                Effect::Broadcast(Event::Settings(settings(55))),
            ]
        );
        assert_eq!(core.on_user_volume(55), []);
        // Soloist's range is 0–100: what is saved is what plays.
        core.on_user_volume(250);
        assert_eq!(core.state.settings.volume, 100);
    }

    #[test]
    fn track_sets_liked_and_checks() {
        let mut core = core();
        core.set_liked_set(["spotify:track:t".to_string()]);
        let effects = core.on_playback(&PlaybackEvent::Track(track("spotify:track:t")));
        assert_eq!(core.state.player.liked, Some(true));
        assert_eq!(checks(&effects), ["spotify:track:t"]);

        let other = core.on_playback(&PlaybackEvent::Track(track("spotify:track:u")));
        assert_eq!(core.state.player.liked, Some(false));
        assert_eq!(checks(&other), ["spotify:track:u"]);
    }

    #[test]
    fn same_track_is_checked_once() {
        // Every playback_state frame repeats the track; one check per track is enough.
        let mut core = core();
        core.on_playback(&PlaybackEvent::Track(track("spotify:track:t")));
        let again = core.on_playback(&PlaybackEvent::Track(track("spotify:track:t")));
        assert_eq!(again, []);
    }

    #[test]
    fn remote_track_mirrored() {
        let mut core = core();
        let t = track("spotify:track:t");
        core.on_playback(&PlaybackEvent::Active(false));
        core.on_playback(&PlaybackEvent::Track(t.clone()));
        core.on_playback(&PlaybackEvent::Status(PlayStatus::Playing));
        let p = &core.state.player;
        assert!(!p.active_here);
        assert_eq!(p.track.as_ref(), Some(&t));
        assert_eq!(p.status, PlayStatus::Playing);
    }

    #[test]
    fn one_broadcast_per_event() {
        let mut core = core();
        let events = [
            PlaybackEvent::Active(true),
            PlaybackEvent::Track(track("spotify:track:t")),
            PlaybackEvent::Status(PlayStatus::Playing),
            PlaybackEvent::Position(Position {
                position_ms: 6_684,
                timestamp_ms: 1_793_915_177_894,
                speed: 1.0,
            }),
            PlaybackEvent::Volume(100),
            PlaybackEvent::Context(Some("spotify:playlist:p".into())),
            PlaybackEvent::Options {
                shuffle: true,
                repeat: Repeat::Context,
            },
            PlaybackEvent::Actions(Actions {
                pause: true,
                skip_next: true,
                ..Actions::default()
            }),
            PlaybackEvent::Quality(StreamQuality {
                codec: "flac".into(),
                bitrate_bps: 1_411_000,
                sample_rate_hz: 44_100,
            }),
        ];
        for ev in &events {
            let effects = core.on_playback(ev);
            assert_eq!(
                players(&effects),
                [&core.state.player],
                "{ev:?} should broadcast the new player state once"
            );
        }
        // The same events again change nothing, so nothing is broadcast.
        for ev in &events {
            let effects = core.on_playback(ev);
            assert_eq!(players(&effects).len(), 0, "{ev:?} changed nothing");
        }
    }

    #[test]
    fn every_player_field_mapped() {
        let mut core = core();
        let pos = Position {
            position_ms: 1_000,
            timestamp_ms: 2_000,
            speed: 1.0,
        };
        let quality = StreamQuality {
            codec: "flac".into(),
            bitrate_bps: 1_411_000,
            sample_rate_hz: 44_100,
        };
        let actions = Actions {
            play: true,
            seek: true,
            ..Actions::default()
        };
        for ev in [
            PlaybackEvent::Status(PlayStatus::Paused),
            PlaybackEvent::Position(pos),
            PlaybackEvent::Quality(quality.clone()),
            PlaybackEvent::Context(Some("spotify:album:b".into())),
            PlaybackEvent::Options {
                shuffle: true,
                repeat: Repeat::Track,
            },
            PlaybackEvent::Actions(actions),
        ] {
            core.on_playback(&ev);
        }
        let p = &core.state.player;
        assert_eq!(p.status, PlayStatus::Paused);
        assert_eq!(p.position, Some(pos));
        assert_eq!(p.quality, Some(quality));
        assert_eq!(p.context_uri.as_deref(), Some("spotify:album:b"));
        assert!(p.shuffle);
        assert_eq!(p.repeat, Repeat::Track);
        assert_eq!(p.actions, actions);

        core.on_playback(&PlaybackEvent::Context(None));
        assert_eq!(core.state.player.context_uri, None);
    }

    #[test]
    fn track_change_keeps_context() {
        // A playback_state without `context` sends no Context event: the last one stands.
        let mut core = core();
        core.on_playback(&PlaybackEvent::Context(Some("spotify:playlist:p".into())));
        core.on_playback(&PlaybackEvent::Track(track("spotify:track:t")));
        assert_eq!(
            core.state.player.context_uri.as_deref(),
            Some("spotify:playlist:p")
        );
    }

    #[test]
    fn upcoming_becomes_queue() {
        let mut core = core();
        let next = vec![track("spotify:track:u"), track("spotify:track:v")];
        let effects = core.on_playback(&PlaybackEvent::Upcoming(next.clone()));
        assert_eq!(core.state.queue, next);
        assert_eq!(effects, [Effect::Broadcast(Event::Queue(next.clone()))]);
        assert_eq!(core.on_playback(&PlaybackEvent::Upcoming(next)), []);
    }

    #[test]
    fn error_becomes_notice() {
        let mut core = core();
        let effects = core.on_playback(&PlaybackEvent::Error("Track unavailable".into()));
        let notice = Notice {
            id: 1,
            level: Level::Error,
            text: "Track unavailable".into(),
        };
        assert_eq!(
            effects,
            [
                Effect::Broadcast(Event::Notice(notice.clone())),
                Effect::NotifyIfClosed(notice),
            ]
        );
        let second = core.on_playback(&PlaybackEvent::Error("again".into()));
        let Effect::Broadcast(Event::Notice(n)) = &second[0] else {
            panic!("{second:?}");
        };
        assert_eq!(n.id, 2);
        assert_eq!(core.notice(Level::Info, "hi").id, 3);
    }

    #[test]
    fn expiry_recorded_and_broadcast() {
        let mut core = core();
        let effects = core.on_playback(&PlaybackEvent::ExpiresInDays(9));
        assert_eq!(core.state.soloist_expires_in_days, Some(9));
        assert_eq!(effects, [Effect::Broadcast(Event::Expiry { days: 9 })]);
        assert_eq!(core.on_playback(&PlaybackEvent::ExpiresInDays(9)), []);
        let fewer = core.on_playback(&PlaybackEvent::ExpiresInDays(8));
        assert_eq!(fewer, [Effect::Broadcast(Event::Expiry { days: 8 })]);
    }

    #[test]
    fn auth_and_exit_left_to_supervisor() {
        let mut core = core();
        let before = core.state.clone();
        assert_eq!(
            core.on_playback(&PlaybackEvent::Auth { logged_in: false }),
            []
        );
        assert_eq!(
            core.on_playback(&PlaybackEvent::Exited {
                code: Some(1),
                tail: "boom".into(),
            }),
            []
        );
        assert_eq!(core.state, before);
    }

    /// Final review I2: Soloist stopped (crashed, restarting): the player shows paused
    /// where it had got to, broadcast once, so MPRIS, the tray and the windows agree.
    #[test]
    fn player_down_pauses_where_it_had_got_to() {
        let mut core = core();
        core.on_playback(&PlaybackEvent::Track(track("spotify:track:t")));
        core.on_playback(&PlaybackEvent::Status(PlayStatus::Playing));
        core.on_playback(&PlaybackEvent::Position(Position {
            position_ms: 10_000,
            timestamp_ms: 1_000_000,
            speed: 1.0,
        }));
        let effects = core.on_player_down(1_030_000);
        let p = &core.state.player;
        assert_eq!(p.status, PlayStatus::Paused);
        assert_eq!(p.position.map(|p| p.position_ms), Some(40_000));
        assert_eq!(players(&effects), [p]);
        assert_eq!(core.on_player_down(1_090_000), [], "once");
    }

    /// Final review C1: before Liked Songs has been read, a track's heart is unknown,
    /// not "not liked"; what a check or a press found out is known; the first read
    /// replaces all of it and says so, for clients to ask again.
    #[test]
    fn liked_state_is_unknown_until_liked_songs_is_read() {
        let mut core = core();
        assert_eq!(core.is_liked("spotify:track:t"), None);
        core.on_playback(&PlaybackEvent::Track(track("spotify:track:t")));
        assert_eq!(core.state.player.liked, None, "not known yet");

        // A check's answer is known before the read.
        core.on_liked("spotify:track:u", false);
        assert_eq!(core.is_liked("spotify:track:u"), Some(false));
        assert_eq!(core.is_liked("spotify:track:t"), None);
        assert!(!core.liked_filled());

        // The first read: everything is known, the player follows, and clients hear it.
        let effects = core.set_liked_set(["spotify:track:t".to_string()]);
        assert!(core.liked_filled());
        assert_eq!(core.is_liked("spotify:track:t"), Some(true));
        assert_eq!(core.is_liked("spotify:track:v"), Some(false));
        assert_eq!(core.state.player.liked, Some(true));
        assert_eq!(players(&effects), [&core.state.player]);
        assert!(effects.contains(&Effect::Broadcast(Event::LikedSetReady)));
    }

    #[test]
    fn liking_current_track_updates_player() {
        let mut core = core();
        core.on_playback(&PlaybackEvent::Track(track("spotify:track:t")));
        assert_eq!(core.state.player.liked, None);

        let effects = core.on_liked("spotify:track:t", true);
        assert_eq!(core.is_liked("spotify:track:t"), Some(true));
        assert_eq!(core.state.player.liked, Some(true));
        assert_eq!(
            effects,
            [
                Effect::Broadcast(Event::LikedChanged {
                    uri: "spotify:track:t".into(),
                    liked: true,
                }),
                Effect::Broadcast(Event::Player(core.state.player.clone())),
            ]
        );

        // A check that confirms what is known still reports it, but the player is unchanged.
        let confirm = core.on_liked("spotify:track:t", true);
        assert_eq!(
            confirm,
            [Effect::Broadcast(Event::LikedChanged {
                uri: "spotify:track:t".into(),
                liked: true,
            })]
        );

        core.on_liked("spotify:track:t", false);
        assert_eq!(core.is_liked("spotify:track:t"), Some(false));
        assert_eq!(core.state.player.liked, Some(false));
    }

    #[test]
    fn liking_other_track_leaves_player() {
        let mut core = core();
        core.on_playback(&PlaybackEvent::Track(track("spotify:track:t")));
        let effects = core.on_liked("spotify:track:u", true);
        assert_eq!(core.is_liked("spotify:track:u"), Some(true));
        assert_eq!(core.state.player.liked, None);
        assert_eq!(
            effects,
            [Effect::Broadcast(Event::LikedChanged {
                uri: "spotify:track:u".into(),
                liked: true,
            })]
        );
    }

    #[test]
    fn liked_set_rederives_current_track() {
        let mut core = core();
        // Nothing playing: the set changes, the player doesn't.
        let ready = Effect::Broadcast(Event::LikedSetReady);
        assert_eq!(
            core.set_liked_set(["spotify:track:t".to_string()]),
            std::slice::from_ref(&ready)
        );
        assert_eq!(core.state.player.liked, None);

        core.on_playback(&PlaybackEvent::Track(track("spotify:track:t")));
        assert_eq!(core.state.player.liked, Some(true));

        // The same membership again: no change, no broadcast.
        let same =
            core.set_liked_set(["spotify:track:t".to_string(), "spotify:track:v".to_string()]);
        assert_eq!(same, [ready]);
        assert_eq!(core.is_liked("spotify:track:v"), Some(true));

        // Replaced, not merged: the current track dropped out.
        let dropped = core.set_liked_set(["spotify:track:u".to_string()]);
        assert_eq!(core.state.player.liked, Some(false));
        assert_eq!(core.is_liked("spotify:track:v"), Some(false));
        assert_eq!(players(&dropped), [&core.state.player]);
    }

    #[test]
    fn initial_state() {
        let core = core();
        let s = &core.state;
        assert_eq!(s.settings, settings(40));
        assert_eq!(s.player.track, None);
        assert_eq!(s.player.status, PlayStatus::Stopped);
        assert!(!s.player.active_here);
        assert_eq!(s.player.volume, 40);
        assert_eq!(s.player.liked, None);
        assert!(s.queue.is_empty());
        assert_eq!(s.soloist, SoloistHealth::Starting);
        assert_eq!(s.session, SessionHealth::Connecting);
        assert_eq!(s.soloist_expires_in_days, None);
    }

    /// The `LikedChanged` broadcasts among `effects`, in order.
    fn liked_changes(effects: &[Effect]) -> Vec<(&str, bool)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Broadcast(Event::LikedChanged { uri, liked }) => {
                    Some((uri.as_str(), *liked))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn refreshed_liked_set_reports_each_change() {
        let mut core = core();
        core.on_playback(&PlaybackEvent::Track(track("spotify:track:b")));
        core.set_liked_set(["spotify:track:a".to_string(), "spotify:track:c".to_string()]);
        assert_eq!(core.state.player.liked, Some(false));

        // b liked elsewhere, c unliked elsewhere, a unchanged.
        let effects =
            core.refresh_liked_set(["spotify:track:b".to_string(), "spotify:track:a".to_string()]);
        assert_eq!(
            liked_changes(&effects),
            [("spotify:track:b", true), ("spotify:track:c", false)]
        );
        // b is playing: the player follows, once, after the changes.
        assert_eq!(players(&effects).len(), 1);
        assert!(matches!(
            effects.last(),
            Some(Effect::Broadcast(Event::Player(_)))
        ));
        assert_eq!(core.state.player.liked, Some(true));
        assert_eq!(core.is_liked("spotify:track:a"), Some(true));
        assert_eq!(core.is_liked("spotify:track:c"), Some(false));

        // The same set again changes nothing.
        let again =
            core.refresh_liked_set(["spotify:track:a".to_string(), "spotify:track:b".to_string()]);
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn settings_change_saves_and_broadcasts() {
        let mut core = core();
        let effects = core.on_setting(Setting::Autostart(true));
        assert!(core.state.settings.autostart);
        assert_eq!(
            effects,
            [
                Effect::SaveSettings,
                Effect::Broadcast(Event::Settings(core.state.settings.clone())),
            ]
        );
        assert!(core.on_setting(Setting::Autostart(true)).is_empty());

        let effects = core.on_setting(Setting::OutputDevice(Some("easyeffects_sink".into())));
        assert_eq!(
            core.state.settings.output_device.as_deref(),
            Some("easyeffects_sink")
        );
        assert_eq!(
            effects[..2],
            [
                Effect::SaveSettings,
                Effect::Broadcast(Event::Settings(core.state.settings.clone())),
            ]
        );
        assert!(matches!(
            &effects[2..],
            [Effect::Broadcast(Event::Notice(Notice { level: Level::Info, text, .. }))]
                if text == OUTPUT_DEVICE_NOTICE
        ));
        assert!(
            core.on_setting(Setting::OutputDevice(Some("easyeffects_sink".into())))
                .is_empty()
        );
        // The volume is untouched by either.
        assert_eq!(core.state.settings.volume, 40);
    }
}
