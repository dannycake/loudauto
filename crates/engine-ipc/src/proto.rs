//! The messages. A client sends `ClientMsg`s, the engine `ServerMsg`s, one per frame.
//! A connection opens with `Hello`, answered by `Welcome` carrying the whole
//! `EngineState`; after that the engine sends `Reply`s to `Request`s (matched by `id`)
//! and `Event`s, the deltas to that state.
//!
//! Every enum is tagged `"t"` with its variant's name. Where all variants are units or
//! structs the fields sit beside the tag; the rest put them under `"c"` (serde's adjacent
//! tagging), since a variant wrapping a list, an option, a scalar or another tagged enum
//! can't share an object with the tag.

use playback::{
    command::Repeat,
    event::{Actions, NowPlaying, PlayStatus, Position, StreamQuality},
};
use serde::{Deserialize, Serialize};
use spot_core::{
    model::RootEntry,
    pages::{
        AlbumPage, ArtistPage, Card, CreditRole, Home, LikedPage, Lyrics, PlaylistPage,
        SearchResults,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ClientMsg {
    /// The first frame on a connection.
    Hello {
        version: u32,
        role: Role,
    },
    Request {
        id: u64,
        req: Request,
    },
}

/// Which kind of client is connecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Role {
    Window,
    Cli,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum ServerMsg {
    /// The answer to `Hello`: the engine's protocol version and the state to start from.
    Welcome {
        version: u32,
        state: Box<EngineState>,
    },
    /// The answer to the request with this `id`; `Err` is the engine's message.
    Reply {
        id: u64,
        result: Result<Reply, String>,
    },
    Event(Event),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum Request {
    Play,
    Pause,
    Next,
    Prev,
    Seek {
        position_ms: u64,
    },
    SetVolume {
        volume: u8,
    },
    SetShuffle {
        on: bool,
    },
    SetRepeat {
        mode: Repeat,
    },
    PlayUri {
        uri: String,
    },
    /// Play `context_uri` (a playlist, an album, …) from `track_uri`; `track_uid` tells
    /// two copies of a track in one playlist apart.
    PlayContext {
        context_uri: String,
        track_uri: Option<String>,
        track_uid: Option<String>,
        shuffle: Option<bool>,
    },
    AddToQueue {
        uri: String,
    },
    StartRadio {
        seed_uri: String,
    },
    /// Like or unlike `uri`, or the current track when `None`, whichever it isn't now;
    /// answered `Reply::Liked`. For the global key, the tray and the CLI, which show no
    /// heart of their own.
    ToggleHeart {
        uri: Option<String>,
    },
    /// Make `uri` liked or not, as the heart the owner clicked asked: the window's
    /// hearts send the state they want, so a heart the window drew from an unknown or
    /// stale state can never write the opposite. Answered `Reply::Liked`.
    SetLiked {
        uri: String,
        liked: bool,
    },
    /// Whether each of `uris` is liked; answered `Reply::LikedState`, in the same order.
    LikedState {
        uris: Vec<String>,
    },
    /// A page; a `prefetch` may be answered `Reply::Dropped` when the network is busy.
    Page {
        req: PageRequest,
        prefetch: bool,
    },
    Lyrics {
        track_uri: String,
    },
    Credits {
        track_uri: String,
    },
    StartLogin,
    SetSetting(Setting),
    ForgetSoloistKey,
    RestartPlayer,
    /// Stop playback, Soloist and the engine.
    Quit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum Reply {
    Ok,
    Liked {
        uri: String,
        liked: bool,
    },
    /// One answer per uri asked about: `None` while the engine doesn't know yet (its
    /// liked set is still being read; `Event::LikedSetReady` follows).
    LikedState(Vec<Option<bool>>),
    Page(PageReply),
    /// `None` when the track has no lyrics.
    Lyrics(Option<Lyrics>),
    Credits(Vec<CreditRole>),
    /// A prefetch the engine didn't make.
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum PageRequest {
    Home,
    Search {
        query: String,
    },
    Artist {
        uri: String,
    },
    Album {
        uri: String,
    },
    Playlist {
        uri: String,
    },
    Liked,
    Recents,
    /// The user's playlists and folders.
    Sidebar,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum Page {
    Home(Home),
    Search(SearchResults),
    Artist(ArtistPage),
    Album(AlbumPage),
    Playlist(PlaylistPage),
    Liked(LikedPage),
    Recents(Vec<Card>),
    Sidebar(Vec<RootEntry>),
}

/// A page and how fresh it is: `stale` marks a cached copy not (yet) refreshed, and
/// `error` is why the last refresh failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageReply {
    pub page: Page,
    /// Unix milliseconds.
    pub fetched_at_ms: i64,
    pub stale: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum Event {
    Player(PlayerState),
    /// What plays next, in order.
    Queue(Vec<NowPlaying>),
    Soloist(SoloistHealth),
    Session(SessionHealth),
    PageUpdated {
        req: PageRequest,
        reply: PageReply,
    },
    LikedChanged {
        uri: String,
        liked: bool,
    },
    Notice(Notice),
    Login(LoginStep),
    Settings(Settings),
    /// Soloist's build expires in this many days.
    Expiry {
        days: u32,
    },
    /// Close the window (another one is opening).
    CloseWindow,
    /// The account's name, once the session has logged in (`EngineState.username`).
    Username(String),
    /// The engine has read Liked Songs whole for the first time: `LikedState` answers
    /// that were `None` are known now, so ask again.
    LikedSetReady,
}

/// Everything a client needs to draw before any event arrives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineState {
    pub player: PlayerState,
    pub queue: Vec<NowPlaying>,
    pub soloist: SoloistHealth,
    pub session: SessionHealth,
    pub login: LoginStep,
    pub settings: Settings,
    pub username: Option<String>,
    pub soloist_expires_in_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    pub track: Option<NowPlaying>,
    pub status: PlayStatus,
    /// Whether this device is the account's active player.
    pub active_here: bool,
    pub context_uri: Option<String>,
    pub position: Option<Position>,
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub actions: Actions,
    /// Whether `track` is liked, once known.
    pub liked: Option<bool>,
    pub quality: Option<StreamQuality>,
}

impl PlayerState {
    /// The player once what played stopped answering (Soloist stopped, or the link to the
    /// engine went): paused, its position frozen where it had got to at `now_ms`, so no
    /// clock counts on and nothing says Playing. Unchanged when it wasn't playing.
    pub fn halted(&self, now_ms: i64) -> PlayerState {
        let mut halted = self.clone();
        if matches!(self.status, PlayStatus::Playing | PlayStatus::Buffering) {
            halted.status = PlayStatus::Paused;
            halted.position = self.position.map(|p| Position {
                position_ms: p.at(now_ms),
                timestamp_ms: now_ms,
                speed: p.speed,
            });
        }
        halted
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum SoloistHealth {
    Starting,
    Ready,
    NotPaired,
    Reconnecting { attempt: u32, retry_in_ms: u64 },
    Failed { message: String, log_tail: String },
    Expired { path: String },
    BinaryMissing { path: String },
}

/// The librespot session's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum SessionHealth {
    Connecting,
    Ready,
    Lost { retry_in_ms: u64, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum LoginStep {
    LoggedIn,
    Needed,
    /// Open `url` and enter `code`.
    Prompt {
        url: String,
        code: String,
    },
    Failed(String),
}

/// A message for the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub id: u64,
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Level {
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub volume: u8,
    /// `None` is the system default.
    pub output_device: Option<String>,
    pub autostart: bool,
}

/// One setting to change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum Setting {
    OutputDevice(Option<String>),
    Autostart(bool),
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, mem::discriminant};

    use serde::de::DeserializeOwned;
    use spot_core::{
        model::PlaylistRef,
        pages::{CardKind, Image, LyricLine, Named, Section, TrackRow},
    };

    use super::*;

    /// JSON there and back, through the wire's own wrappers.
    fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
        let json = serde_json::to_string(value).unwrap_or_else(|e| panic!("{value:?}: {e}"));
        let back: T = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(&back, value, "{json}");
    }

    /// Asserts `values` hold one value of each of `count` variants.
    fn one_per_variant<T>(values: &[T], count: usize) {
        let kinds: HashSet<_> = values.iter().map(discriminant).collect();
        assert_eq!(kinds.len(), values.len(), "two values of one variant");
        assert_eq!(kinds.len(), count, "a variant has no value");
    }

    fn track() -> NowPlaying {
        NowPlaying {
            uri: "spotify:track:t".into(),
            title: "nerve".into(),
            artists: vec!["2hollis".into()],
            artist_uris: vec!["spotify:artist:a".into()],
            album: Some("boy".into()),
            album_uri: Some("spotify:album:b".into()),
            duration_ms: Some(150_000),
            cover_url: Some("https://i.scdn.co/image/x".into()),
        }
    }

    fn player() -> PlayerState {
        PlayerState {
            track: Some(track()),
            status: PlayStatus::Other("odd".into()),
            active_here: true,
            context_uri: Some("spotify:playlist:p".into()),
            position: Some(Position {
                position_ms: 1_000,
                timestamp_ms: 1_762_400_000_000,
                speed: 1.0,
            }),
            volume: 40,
            shuffle: true,
            repeat: Repeat::Context,
            actions: Actions {
                pause: true,
                seek: true,
                ..Actions::default()
            },
            liked: Some(false),
            quality: Some(StreamQuality {
                codec: "flac".into(),
                bitrate_bps: 1_411_000,
                sample_rate_hz: 44_100,
            }),
        }
    }

    fn image() -> Vec<Image> {
        vec![Image {
            url: "https://i.scdn.co/image/c".into(),
            width: Some(300),
        }]
    }

    fn card() -> Card {
        Card {
            uri: "spotify:album:b".into(),
            kind: CardKind::Album,
            title: "boy".into(),
            subtitle: "2hollis".into(),
            images: image(),
        }
    }

    fn row() -> TrackRow {
        TrackRow {
            uri: "spotify:track:t".into(),
            uid: Some("u1".into()),
            title: "nerve".into(),
            artists: vec![Named {
                uri: "spotify:artist:a".into(),
                name: "2hollis".into(),
            }],
            album: None,
            duration_ms: 150_000,
            images: vec![],
            added_at: Some("2025-07-29T15:40:21Z".into()),
        }
    }

    fn pages() -> Vec<Page> {
        vec![
            Page::Home(Home {
                greeting: "Good evening".into(),
                sections: vec![Section {
                    title: "Recents".into(),
                    cards: vec![card()],
                }],
            }),
            Page::Search(SearchResults {
                tracks: vec![row()],
                artists: vec![],
                albums: vec![card()],
                playlists: vec![],
            }),
            Page::Artist(ArtistPage {
                uri: "spotify:artist:a".into(),
                name: "2hollis".into(),
                header: vec![],
                avatar: image(),
                top_tracks: vec![row()],
                albums: vec![card()],
                singles: vec![],
                related: vec![],
            }),
            Page::Album(AlbumPage {
                uri: "spotify:album:b".into(),
                name: "boy".into(),
                artists: vec![],
                release_date: "2023".into(),
                label: "label".into(),
                images: image(),
                total: 1,
                tracks: vec![row()],
            }),
            Page::Playlist(PlaylistPage {
                uri: "spotify:playlist:p".into(),
                name: "Main".into(),
                description: "".into(),
                images: vec![],
                total: 1,
                tracks: vec![row()],
            }),
            Page::Liked(LikedPage {
                total: 1,
                tracks: vec![row()],
            }),
            Page::Recents(vec![card()]),
            Page::Sidebar(vec![
                RootEntry::FolderStart {
                    id: "f".into(),
                    name: "Folder".into(),
                },
                RootEntry::Playlist(PlaylistRef {
                    uri: "spotify:playlist:p".into(),
                    name: "Main".into(),
                }),
                RootEntry::FolderEnd { id: "f".into() },
            ]),
        ]
    }

    fn page_reply(page: Page) -> PageReply {
        PageReply {
            page,
            fetched_at_ms: 1_762_400_000_000,
            stale: true,
            error: Some("Spotify didn't answer in 15 s.".into()),
        }
    }

    fn page_requests() -> Vec<PageRequest> {
        vec![
            PageRequest::Home,
            PageRequest::Search {
                query: "hollis".into(),
            },
            PageRequest::Artist {
                uri: "spotify:artist:a".into(),
            },
            PageRequest::Album {
                uri: "spotify:album:b".into(),
            },
            PageRequest::Playlist {
                uri: "spotify:playlist:p".into(),
            },
            PageRequest::Liked,
            PageRequest::Recents,
            PageRequest::Sidebar,
        ]
    }

    fn requests() -> Vec<Request> {
        vec![
            Request::Play,
            Request::Pause,
            Request::Next,
            Request::Prev,
            Request::Seek {
                position_ms: 61_000,
            },
            Request::SetVolume { volume: 40 },
            Request::SetShuffle { on: true },
            Request::SetRepeat {
                mode: Repeat::Track,
            },
            Request::PlayUri {
                uri: "spotify:track:t".into(),
            },
            Request::PlayContext {
                context_uri: "spotify:playlist:p".into(),
                track_uri: Some("spotify:track:t".into()),
                track_uid: None,
                shuffle: Some(false),
            },
            Request::AddToQueue {
                uri: "spotify:track:t".into(),
            },
            Request::StartRadio {
                seed_uri: "spotify:track:t".into(),
            },
            Request::ToggleHeart { uri: None },
            Request::SetLiked {
                uri: "spotify:track:t".into(),
                liked: false,
            },
            Request::LikedState {
                uris: vec!["spotify:track:t".into(), "spotify:track:u".into()],
            },
            Request::Page {
                req: PageRequest::Search {
                    query: "hollis".into(),
                },
                prefetch: true,
            },
            Request::Lyrics {
                track_uri: "spotify:track:t".into(),
            },
            Request::Credits {
                track_uri: "spotify:track:t".into(),
            },
            Request::StartLogin,
            Request::SetSetting(Setting::OutputDevice(Some("null-sink".into()))),
            Request::ForgetSoloistKey,
            Request::RestartPlayer,
            Request::Quit,
        ]
    }

    fn replies() -> Vec<Reply> {
        vec![
            Reply::Ok,
            Reply::Liked {
                uri: "spotify:track:t".into(),
                liked: true,
            },
            Reply::LikedState(vec![Some(true), Some(false), None]),
            Reply::Page(page_reply(Page::Recents(vec![card()]))),
            Reply::Lyrics(Some(Lyrics {
                synced: true,
                provider: "MusixMatch".into(),
                lines: vec![LyricLine {
                    start_ms: 1_200,
                    words: "words".into(),
                }],
            })),
            Reply::Credits(vec![CreditRole {
                role: "Writers".into(),
                names: vec!["a".into()],
            }]),
            Reply::Dropped,
        ]
    }

    fn soloist_healths() -> Vec<SoloistHealth> {
        vec![
            SoloistHealth::Starting,
            SoloistHealth::Ready,
            SoloistHealth::NotPaired,
            SoloistHealth::Reconnecting {
                attempt: 1,
                retry_in_ms: 1_000,
            },
            SoloistHealth::Failed {
                message: "crashed".into(),
                log_tail: "boom".into(),
            },
            SoloistHealth::Expired {
                path: "/x/soloist".into(),
            },
            SoloistHealth::BinaryMissing {
                path: "/x/soloist".into(),
            },
        ]
    }

    fn session_healths() -> Vec<SessionHealth> {
        vec![
            SessionHealth::Connecting,
            SessionHealth::Ready,
            SessionHealth::Lost {
                retry_in_ms: 5_000,
                reason: "dns".into(),
            },
        ]
    }

    fn login_steps() -> Vec<LoginStep> {
        vec![
            LoginStep::LoggedIn,
            LoginStep::Needed,
            LoginStep::Prompt {
                url: "https://accounts.spotify.com/x".into(),
                code: "ABCD".into(),
            },
            LoginStep::Failed("denied".into()),
        ]
    }

    fn settings() -> Settings {
        Settings {
            volume: 40,
            output_device: None,
            autostart: true,
        }
    }

    fn events() -> Vec<Event> {
        vec![
            Event::Player(player()),
            Event::Queue(vec![track(), track()]),
            Event::Soloist(SoloistHealth::Reconnecting {
                attempt: 2,
                retry_in_ms: 2_000,
            }),
            Event::Session(SessionHealth::Lost {
                retry_in_ms: 5_000,
                reason: "dns".into(),
            }),
            Event::PageUpdated {
                req: PageRequest::Liked,
                reply: page_reply(Page::Liked(LikedPage {
                    total: 1,
                    tracks: vec![row()],
                })),
            },
            Event::LikedChanged {
                uri: "spotify:track:t".into(),
                liked: false,
            },
            Event::Notice(Notice {
                id: 3,
                level: Level::Info,
                text: "Switched the player to lossless; it restarted once.".into(),
            }),
            Event::Login(LoginStep::Failed("denied".into())),
            Event::Settings(settings()),
            Event::Expiry { days: 89 },
            Event::CloseWindow,
            Event::Username("someone".into()),
            Event::LikedSetReady,
        ]
    }

    fn state() -> EngineState {
        EngineState {
            player: player(),
            queue: vec![track()],
            soloist: SoloistHealth::Ready,
            session: SessionHealth::Ready,
            login: LoginStep::Prompt {
                url: "https://accounts.spotify.com/x".into(),
                code: "ABCD".into(),
            },
            settings: settings(),
            username: Some("someone".into()),
            soloist_expires_in_days: Some(89),
        }
    }

    #[test]
    fn every_request_roundtrips() {
        let all = requests();
        one_per_variant(&all, 23);
        for (id, req) in all.into_iter().enumerate() {
            roundtrip(&req);
            roundtrip(&ClientMsg::Request { id: id as u64, req });
        }
    }

    #[test]
    fn hello_roundtrips() {
        for role in [Role::Window, Role::Cli] {
            roundtrip(&ClientMsg::Hello { version: 1, role });
        }
    }

    #[test]
    fn every_reply_roundtrips() {
        let all = replies();
        one_per_variant(&all, 7);
        for (id, reply) in all.into_iter().enumerate() {
            roundtrip(&reply);
            roundtrip(&ServerMsg::Reply {
                id: id as u64,
                result: Ok(reply),
            });
        }
        roundtrip(&ServerMsg::Reply {
            id: 9,
            result: Err("Couldn't like: 500".into()),
        });
    }

    #[test]
    fn every_page_roundtrips() {
        let all = pages();
        one_per_variant(&all, 8);
        for page in all {
            roundtrip(&page);
            roundtrip(&Reply::Page(page_reply(page)));
        }
    }

    #[test]
    fn every_page_request_roundtrips() {
        let all = page_requests();
        one_per_variant(&all, 8);
        for req in all {
            roundtrip(&req);
            roundtrip(&Request::Page {
                req,
                prefetch: false,
            });
        }
    }

    #[test]
    fn every_event_roundtrips() {
        let all = events();
        one_per_variant(&all, 13);
        for event in all {
            roundtrip(&event);
            roundtrip(&ServerMsg::Event(event));
        }
    }

    #[test]
    fn every_health_and_login_step_roundtrips() {
        let soloist = soloist_healths();
        one_per_variant(&soloist, 7);
        for health in soloist {
            roundtrip(&ServerMsg::Event(Event::Soloist(health)));
        }
        let session = session_healths();
        one_per_variant(&session, 3);
        for health in session {
            roundtrip(&ServerMsg::Event(Event::Session(health)));
        }
        let steps = login_steps();
        one_per_variant(&steps, 4);
        for step in steps {
            roundtrip(&ServerMsg::Event(Event::Login(step)));
        }
        for setting in [Setting::OutputDevice(None), Setting::Autostart(false)] {
            roundtrip(&ClientMsg::Request {
                id: 1,
                req: Request::SetSetting(setting),
            });
        }
    }

    #[test]
    fn welcome_roundtrips() {
        roundtrip(&ServerMsg::Welcome {
            version: 1,
            state: Box::new(state()),
        });
    }

    /// Final review I2: a player whose Soloist (or engine) stopped answering shows paused
    /// where it had got to, so nothing keeps counting or says Playing.
    #[test]
    fn halted_player_is_paused_where_it_had_got_to() {
        let mut playing = player();
        playing.status = PlayStatus::Playing;
        let halted = playing.halted(1_762_400_005_000);
        assert_eq!(halted.status, PlayStatus::Paused);
        assert_eq!(
            halted.position,
            Some(Position {
                position_ms: 6_000,
                timestamp_ms: 1_762_400_005_000,
                speed: 1.0,
            })
        );
        assert_eq!(halted.track, playing.track, "the rest is kept");
        let mut buffering = player();
        buffering.status = PlayStatus::Buffering;
        assert_eq!(buffering.halted(1).status, PlayStatus::Paused);
        // Not playing: nothing to stop.
        let mut paused = player();
        paused.status = PlayStatus::Paused;
        assert_eq!(paused.halted(1_762_400_005_000), paused);
    }

    #[test]
    fn wire_shape() {
        // Pinned so a serde change can't silently alter the protocol.
        let hello = ClientMsg::Hello {
            version: 1,
            role: Role::Cli,
        };
        assert_eq!(
            serde_json::to_value(&hello).unwrap(),
            serde_json::json!({"t": "Hello", "version": 1, "role": {"t": "Cli"}})
        );
        let request = ClientMsg::Request {
            id: 4,
            req: Request::Seek { position_ms: 5 },
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({"t": "Request", "id": 4, "req": {"t": "Seek", "c": {"position_ms": 5}}})
        );
        // An unknown heart is `null`: the engine's liked set isn't read yet.
        let liked = ServerMsg::Reply {
            id: 5,
            result: Ok(Reply::LikedState(vec![Some(true), None])),
        };
        assert_eq!(
            serde_json::to_value(&liked).unwrap(),
            serde_json::json!({"t": "Reply", "c": {"id": 5, "result": {"Ok": {"t": "LikedState", "c": [true, null]}}}})
        );
        let event = ServerMsg::Event(Event::Expiry { days: 3 });
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({"t": "Event", "c": {"t": "Expiry", "c": {"days": 3}}})
        );
    }
}
