//! The one event stream every consumer (GUI, MPRIS, scrobbler) reads, normalized from
//! Soloist's wire messages.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    command::Repeat,
    wire::{Entity, Incoming, WireOptions, WirePosition},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NowPlaying {
    pub uri: String,
    pub title: String,
    pub artists: Vec<String>,
    /// The uri of each of `artists`, in the same order.
    pub artist_uris: Vec<String>,
    pub album: Option<String>,
    /// The item's `parent`: the album, for a track.
    pub album_uri: Option<String>,
    pub duration_ms: Option<u64>,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlayStatus {
    Playing,
    Paused,
    Buffering,
    Stopped,
    Other(String),
}

/// A position anchor: `position_ms` was true at server time `timestamp_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub position_ms: u64,
    pub timestamp_ms: i64,
    pub speed: f64,
}

impl Position {
    /// Position extrapolated to `now_ms` (Unix epoch milliseconds).
    pub fn at(&self, now_ms: i64) -> u64 {
        let elapsed = now_ms.saturating_sub(self.timestamp_ms).max(0) as f64;
        let advanced = (elapsed * self.speed.max(0.0)) as u64;
        self.position_ms.saturating_add(advanced)
    }
}

/// Codec details Soloist logs when it opens a track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamQuality {
    pub codec: String,
    pub bitrate_bps: u32,
    pub sample_rate_hz: u32,
}

/// What the player allows now: each is whether Soloist listed the action in
/// `available_actions` (the captures taken while playing list `pause` but not `play`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Actions {
    pub play: bool,
    pub pause: bool,
    pub skip_next: bool,
    pub skip_prev: bool,
    pub seek: bool,
    pub shuffle: bool,
    pub set_repeat: bool,
    pub add_to_queue: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackEvent {
    Auth {
        logged_in: bool,
    },
    /// Whether *this* device is the account's active player. Soloist reports the account's
    /// playback state even while another device plays it.
    Active(bool),
    Track(NowPlaying),
    Status(PlayStatus),
    Position(Position),
    Volume(u8),
    Upcoming(Vec<NowPlaying>),
    Quality(StreamQuality),
    /// The uri of what is playing from (playlist, album, …); `None` when nothing is.
    Context(Option<String>),
    Options {
        shuffle: bool,
        repeat: Repeat,
    },
    Actions(Actions),
    Error(String),
    /// Soloist's log said its build expires in this many days.
    ExpiresInDays(u32),
    /// The Soloist process ended, whatever ended it (a crash, a kill or `stop`); sent once
    /// per process. `code` is `None` when a signal ended it; `tail` is its last output lines.
    Exited {
        code: Option<i32>,
        tail: String,
    },
}

/// Cover sizes Soloist sends, best first.
const COVER_PREFERENCE: [&str; 4] = ["xlarge", "large", "default", "small"];

pub fn now_playing(item: &Entity) -> NowPlaying {
    let d = &item.decorations;
    let name_of = |e: &Entity| e.decorations.identity.as_ref().map(|i| i.name.clone());
    let covers = d
        .visual_identity
        .as_ref()
        .map(|v| v.cover.as_slice())
        .unwrap_or_default();
    let cover_url = COVER_PREFERENCE
        .iter()
        .find_map(|size| covers.iter().find(|c| c.size == *size))
        .or_else(|| covers.first())
        .map(|c| c.url.clone());
    // A creator without a name can't be shown, so its uri is left out too.
    let (artists, artist_uris) = d
        .creators
        .iter()
        .filter_map(|c| name_of(&c.entity).map(|name| (name, c.entity.uri.clone())))
        .unzip();
    NowPlaying {
        uri: item.uri.clone(),
        title: name_of(item).unwrap_or_default(),
        artists,
        artist_uris,
        album: d.parent.as_ref().and_then(|p| name_of(&p.entity)),
        album_uri: d.parent.as_ref().map(|p| p.entity.uri.clone()),
        duration_ms: d.playback.as_ref().and_then(|p| p.duration_ms),
        cover_url,
    }
}

fn actions(available: &Map<String, Value>) -> Actions {
    let has = |action: &str| available.contains_key(action);
    Actions {
        play: has("play"),
        pause: has("pause"),
        skip_next: has("skip_next"),
        skip_prev: has("skip_prev"),
        seek: has("seek"),
        shuffle: has("shuffle"),
        set_repeat: has("set_repeat"),
        add_to_queue: has("add_to_queue"),
    }
}

/// `None`, logged, for a repeat mode LOUDAUTO doesn't know.
fn options(o: WireOptions) -> Option<PlaybackEvent> {
    let repeat = match o.repeat.as_str() {
        "off" => Repeat::Off,
        "context" => Repeat::Context,
        "track" => Repeat::Track,
        other => {
            log::warn!("Soloist sent an unknown repeat mode {other:?}; options ignored");
            return None;
        }
    };
    Some(PlaybackEvent::Options {
        shuffle: o.shuffle,
        repeat,
    })
}

fn status(s: &str) -> PlayStatus {
    match s {
        "playing" => PlayStatus::Playing,
        "paused" => PlayStatus::Paused,
        "buffering" => PlayStatus::Buffering,
        "stopped" => PlayStatus::Stopped,
        other => PlayStatus::Other(other.to_string()),
    }
}

fn position(p: WirePosition) -> Position {
    Position {
        position_ms: p.position_ms,
        timestamp_ms: p.timestamp_ms,
        speed: p.speed,
    }
}

pub fn normalize(incoming: Incoming) -> Vec<PlaybackEvent> {
    use PlaybackEvent as E;
    match incoming {
        Incoming::AuthState {
            logged_in,
            is_active,
        } => {
            vec![E::Auth { logged_in }, E::Active(is_active)]
        }
        Incoming::PlaybackState {
            status: s,
            item,
            position: pos,
            volume,
            is_active,
            context,
            options: opts,
            available_actions,
        } => {
            let mut events = vec![E::Active(is_active)];
            events.extend(item.as_ref().map(|i| E::Track(now_playing(i))));
            events.extend(s.as_deref().map(|s| E::Status(status(s))));
            events.extend(pos.map(|p| E::Position(position(p))));
            events.extend(volume.map(E::Volume));
            events.extend(context.map(|c| E::Context(Some(c.uri))));
            events.extend(opts.and_then(options));
            events.extend(available_actions.as_ref().map(|a| E::Actions(actions(a))));
            events
        }
        Incoming::TrackChanged { item } => vec![E::Track(now_playing(&item))],
        Incoming::ContextChanged { context } => vec![E::Context(context.map(|c| c.uri))],
        Incoming::OptionsChanged { options: o } => options(o).into_iter().collect(),
        Incoming::PlaybackChanged { status: s } => vec![E::Status(status(&s))],
        Incoming::VolumeChanged { volume } => vec![E::Volume(volume)],
        Incoming::DeviceChanged { is_active } => vec![E::Active(is_active)],
        Incoming::PositionSync { position: p } => vec![E::Position(position(p))],
        Incoming::QueueChanged { upcoming } => {
            vec![E::Upcoming(
                upcoming.iter().map(|q| now_playing(&q.item)).collect(),
            )]
        }
        Incoming::Error { message } => vec![E::Error(message)],
        Incoming::CommandResult { .. } | Incoming::Unknown => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Cover, Decorations, VisualIdentity, parse_incoming};

    const CAPTURE: &str = include_str!("../tests/fixtures/state.json");
    /// The phone was the active player: Soloist still reports the account's state.
    const REMOTE: &str = include_str!("../tests/fixtures/remote_state.json");

    fn frame(i: usize) -> Incoming {
        let raw: Vec<serde_json::Value> = serde_json::from_str(CAPTURE).unwrap();
        parse_incoming(&raw[i].to_string()).unwrap()
    }

    /// The events of one frame; panics if the frame is dropped.
    fn events_of(text: &str) -> Vec<PlaybackEvent> {
        normalize(parse_incoming(text).expect("frame was dropped"))
    }

    fn live_item() -> Entity {
        let Incoming::PlaybackState {
            item: Some(item), ..
        } = frame(1)
        else {
            panic!("second frame has no item");
        };
        item
    }

    fn with_covers(covers: &[(&str, &str)]) -> Entity {
        Entity {
            uri: "spotify:track:x".into(),
            entity_type: None,
            decorations: Decorations {
                visual_identity: Some(VisualIdentity {
                    cover: covers
                        .iter()
                        .map(|(size, url)| Cover {
                            size: (*size).into(),
                            url: (*url).into(),
                        })
                        .collect(),
                }),
                ..Default::default()
            },
        }
    }

    #[test]
    fn playing_elsewhere_is_inactive() {
        let events = normalize(frame(1));
        assert_eq!(events[0], PlaybackEvent::Active(false));
        assert_eq!(
            events[1],
            PlaybackEvent::Track(NowPlaying {
                uri: "spotify:track:1XD4K4CGAKTIBmFpvuaFru".into(),
                title: "Dancing with your eyes closed".into(),
                artists: vec!["Jane Remover".into()],
                artist_uris: vec!["spotify:artist:2rLGlNI6htigNxx172qxLu".into()],
                album: Some("Revengeseekerz".into()),
                album_uri: Some("spotify:album:21b4cDNse2AMpj94ykfuON".into()),
                duration_ms: Some(230526),
                cover_url: Some(
                    "https://i.scdn.co/image/ab67616d0000b273ab0d83b752dcf9f2a7ddd51b".into()
                ),
            })
        );
        assert_eq!(events[2], PlaybackEvent::Status(PlayStatus::Playing));
        assert_eq!(
            events[3],
            PlaybackEvent::Position(Position {
                position_ms: 6684,
                timestamp_ms: 1793915177894,
                speed: 1.0
            })
        );
        assert_eq!(events[4], PlaybackEvent::Volume(100));
        assert_eq!(
            events[5],
            PlaybackEvent::Context(Some("spotify:playlist:37i9dQZF1F5p3rmiWPIYgZ".into()))
        );
        assert_eq!(
            events[6],
            PlaybackEvent::Options {
                shuffle: false,
                repeat: Repeat::Off
            }
        );
        assert_eq!(
            events[7],
            PlaybackEvent::Actions(Actions {
                play: false,
                pause: true,
                skip_next: true,
                skip_prev: false,
                seek: true,
                shuffle: true,
                set_repeat: true,
                add_to_queue: true,
            })
        );
        assert_eq!(events.len(), 8);
    }

    #[test]
    fn auth_state_reports_login_and_activity() {
        assert_eq!(
            normalize(frame(0)),
            vec![
                PlaybackEvent::Auth { logged_in: true },
                PlaybackEvent::Active(false)
            ]
        );
    }

    #[test]
    fn cover_preference() {
        let e = with_covers(&[("small", "a"), ("large", "b"), ("default", "c")]);
        assert_eq!(now_playing(&e).cover_url.as_deref(), Some("b"));
        assert_eq!(now_playing(&with_covers(&[])).cover_url, None);
        let e = with_covers(&[("tiny", "t"), ("huge", "h")]);
        assert_eq!(now_playing(&e).cover_url.as_deref(), Some("t"));
    }

    #[test]
    fn missing_decorations_give_empty_title() {
        let np = now_playing(&with_covers(&[]));
        assert_eq!(np.title, "");
        assert!(np.artists.is_empty());
        assert_eq!(np.album, None);
        assert_eq!(np.duration_ms, None);
    }

    #[test]
    fn position_extrapolates() {
        let p = Position {
            position_ms: 45000,
            timestamp_ms: 1000,
            speed: 1.0,
        };
        assert_eq!(p.at(3500), 47500);
        assert_eq!(p.at(500), 45000);
        assert_eq!(Position { speed: 0.0, ..p }.at(3500), 45000);
    }

    #[test]
    fn status_mapping() {
        let s = |status: &str| {
            normalize(Incoming::PlaybackChanged {
                status: status.into(),
            })
        };
        assert_eq!(
            s("playing"),
            vec![PlaybackEvent::Status(PlayStatus::Playing)]
        );
        assert_eq!(s("paused"), vec![PlaybackEvent::Status(PlayStatus::Paused)]);
        assert_eq!(
            s("buffering"),
            vec![PlaybackEvent::Status(PlayStatus::Buffering)]
        );
        assert_eq!(
            s("stopped"),
            vec![PlaybackEvent::Status(PlayStatus::Stopped)]
        );
        assert_eq!(
            s("weird"),
            vec![PlaybackEvent::Status(PlayStatus::Other("weird".into()))]
        );
    }

    #[test]
    fn error_and_result() {
        assert_eq!(
            normalize(Incoming::Error {
                message: "m".into()
            }),
            vec![PlaybackEvent::Error("m".into())]
        );
        assert_eq!(
            normalize(Incoming::CommandResult {
                command: "pause".into()
            }),
            vec![]
        );
        assert_eq!(normalize(Incoming::Unknown), vec![]);
    }

    #[test]
    fn simple_updates() {
        assert_eq!(
            normalize(Incoming::VolumeChanged { volume: 7 }),
            vec![PlaybackEvent::Volume(7)]
        );
        assert_eq!(
            normalize(Incoming::DeviceChanged { is_active: true }),
            vec![PlaybackEvent::Active(true)]
        );
        let queue = normalize(frame(3));
        assert!(
            matches!(&queue[..], [PlaybackEvent::Upcoming(items)] if items.len() == 3),
            "{queue:?}"
        );
    }

    #[test]
    fn remote_state_frame() {
        assert_eq!(
            events_of(REMOTE),
            vec![
                PlaybackEvent::Active(false),
                PlaybackEvent::Track(NowPlaying {
                    uri: "spotify:track:5kCNaSNJ6AebIX5cM991Tk".into(),
                    title: String::new(),
                    artists: vec![],
                    artist_uris: vec![],
                    album: None,
                    album_uri: None,
                    duration_ms: None,
                    cover_url: None,
                }),
                PlaybackEvent::Status(PlayStatus::Playing),
                PlaybackEvent::Position(Position {
                    position_ms: 196907,
                    timestamp_ms: 1793954713190,
                    speed: 1.0
                }),
                PlaybackEvent::Volume(100),
                PlaybackEvent::Context(Some("spotify:playlist:37i9dQZF1F5p3rmiWPIYgZ".into())),
                PlaybackEvent::Options {
                    shuffle: false,
                    repeat: Repeat::Off
                },
                PlaybackEvent::Actions(Actions {
                    play: false,
                    pause: true,
                    skip_next: true,
                    skip_prev: true,
                    seek: true,
                    shuffle: true,
                    set_repeat: true,
                    add_to_queue: true,
                }),
            ]
        );
    }

    #[test]
    fn options_changed_frame() {
        assert_eq!(
            events_of(
                r#"{"type":"options_changed","options":{"shuffle":true,"repeat":"context"}}"#
            ),
            vec![PlaybackEvent::Options {
                shuffle: true,
                repeat: Repeat::Context
            }]
        );
        assert_eq!(
            events_of(r#"{"type":"options_changed","options":{"shuffle":false,"repeat":"track"}}"#),
            vec![PlaybackEvent::Options {
                shuffle: false,
                repeat: Repeat::Track
            }]
        );
        // A mode LOUDAUTO doesn't know can't be shown, so the frame says nothing.
        assert_eq!(
            events_of(
                r#"{"type":"options_changed","options":{"shuffle":true,"repeat":"sideways"}}"#
            ),
            vec![]
        );
    }

    #[test]
    fn context_changed_frame() {
        assert_eq!(
            events_of(r#"{"type":"context_changed","context":{"uri":"spotify:album:x"}}"#),
            vec![PlaybackEvent::Context(Some("spotify:album:x".into()))]
        );
        assert_eq!(
            events_of(r#"{"type":"context_changed","context":null}"#),
            vec![PlaybackEvent::Context(None)]
        );
        assert_eq!(
            events_of(r#"{"type":"context_changed"}"#),
            vec![PlaybackEvent::Context(None)]
        );
    }

    #[test]
    fn bad_field_does_not_drop_frame() {
        assert_eq!(
            events_of(r#"{"type":"playback_state","status":"paused","volume":"loud"}"#),
            vec![
                PlaybackEvent::Active(false),
                PlaybackEvent::Status(PlayStatus::Paused)
            ]
        );
    }

    #[test]
    fn bad_nested_fields_keep_the_rest() {
        // Everything but the item's uri, its name and the second creator is ill-typed.
        let text = r#"{"type":"playback_state","is_active":"yes","status":7,
            "position":{"position_ms":"x"},"volume":300,"context":{"name":"no uri"},
            "options":{"shuffle":"on","repeat":"off"},"available_actions":[],
            "item":{"uri":"spotify:track:t","entity_type":1,"decorations":{
                "identity":{"name":"T"},"visual_identity":5,"parent":"album",
                "creators":[{"entity":{"decorations":{}}},
                    {"entity":{"uri":"spotify:artist:b","decorations":{"identity":{"name":"B"}}}}],
                "playback":{"duration_ms":"long"}}}}"#;
        assert_eq!(
            events_of(text),
            vec![
                PlaybackEvent::Active(false),
                PlaybackEvent::Track(NowPlaying {
                    uri: "spotify:track:t".into(),
                    title: "T".into(),
                    artists: vec!["B".into()],
                    artist_uris: vec!["spotify:artist:b".into()],
                    album: None,
                    album_uri: None,
                    duration_ms: None,
                    cover_url: None,
                }),
            ]
        );
    }

    #[test]
    fn bad_queue_entry_is_skipped() {
        let text = r#"{"type":"queue_changed","upcoming":[{"item":{}},{"item":{"uri":"spotify:track:q"}}]}"#;
        let events = events_of(text);
        assert!(
            matches!(&events[..], [PlaybackEvent::Upcoming(items)] if items.len() == 1 && items[0].uri == "spotify:track:q"),
            "{events:?}"
        );
    }

    #[test]
    fn now_playing_links() {
        let np = now_playing(&live_item());
        assert_eq!(
            np.album_uri.as_deref(),
            Some("spotify:album:21b4cDNse2AMpj94ykfuON")
        );
        assert_eq!(np.artist_uris, ["spotify:artist:2rLGlNI6htigNxx172qxLu"]);
    }

    #[test]
    fn artist_uris_line_up_with_names() {
        // A creator without a name can't be shown, so its uri is left out too.
        let text = r#"{"type":"track_changed","item":{"uri":"spotify:track:t","decorations":{"creators":[
            {"entity":{"uri":"spotify:artist:a"}},
            {"entity":{"uri":"spotify:artist:b","decorations":{"identity":{"name":"B"}}}}]}}}"#;
        let events = events_of(text);
        let [PlaybackEvent::Track(np)] = &events[..] else {
            panic!("{events:?}");
        };
        assert_eq!(np.artists, ["B"]);
        assert_eq!(np.artist_uris, ["spotify:artist:b"]);
    }

    fn roundtrip<T>(value: T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<T>(&json).unwrap(), value);
    }

    #[test]
    fn ipc_types_roundtrip() {
        roundtrip(now_playing(&live_item()));
        roundtrip(PlayStatus::Other("odd".into()));
        roundtrip(PlayStatus::Buffering);
        roundtrip(Position {
            position_ms: 1,
            timestamp_ms: -2,
            speed: 1.5,
        });
        roundtrip(StreamQuality {
            codec: "flac".into(),
            bitrate_bps: 1_400_000,
            sample_rate_hz: 44_100,
        });
        roundtrip(Repeat::Track);
        roundtrip(Actions {
            pause: true,
            seek: true,
            ..Actions::default()
        });
    }
}
