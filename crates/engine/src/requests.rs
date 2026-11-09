//! What each client request does. Player commands go to Soloist and are answered with
//! its outcome; the heart, pages, lyrics and credits go to Spotify on the User lane; the
//! liked state is answered from the local liked set. Quitting closes the open windows
//! first.

use std::{future::Future, sync::Arc, time::Duration};

use engine_ipc::proto::{Reply, Request, Role, Setting};
use spot_core::{
    SpotError,
    web::{ConnectPlay, lanes::Lane},
};
use tokio::sync::mpsc;

use crate::{
    engine::{Engine, now_ms},
    server::WindowControl,
    session::SessionHint,
    soloist::SoloistCmd,
    window::CLOSE_WAIT,
};

/// The answer to `StartLogin` once the session supervisor is gone.
pub const SESSION_STOPPED: &str = "The Spotify session stopped.";
/// The longest a context play takes to answer: Soloist's device id, then the Connect
/// play command (one 15 s request, its retry after a 401 included). The window's
/// `engine_ipc::client::PLAY_TIMEOUT` outlasts it.
pub const PLAY_CEILING: Duration = Duration::from_secs(20);
/// The longest a radio takes to answer: finding its playlist, then playing it. The
/// window's `engine_ipc::client::RADIO_TIMEOUT` outlasts it.
pub const RADIO_CEILING: Duration = Duration::from_secs(30);
/// What a play or a radio that took longer than its ceiling says.
pub const NO_ANSWER: &str = "Spotify didn't answer in time.";

impl Engine {
    /// Answers `req` from a client of `role`; `Err` is the message to show.
    pub async fn handle(self: &Arc<Self>, role: Role, req: Request) -> Result<Reply, String> {
        use Request as R;
        if let Some(cmd) = player_command(&req) {
            return self.soloist(cmd).await.map(|()| Reply::Ok);
        }
        match req {
            R::SetVolume { volume } => {
                {
                    let mut core = self.core.lock().await;
                    let effects = core.on_user_volume(volume);
                    self.execute(&core, effects);
                }
                self.soloist(SoloistCmd::Volume(volume.min(100)))
                    .await
                    .map(|()| Reply::Ok)
            }
            R::PlayContext {
                context_uri,
                track_uri,
                track_uid,
                shuffle,
            } => {
                within(PLAY_CEILING, async {
                    let current = self.core.lock().await.state.player.shuffle;
                    let play = ConnectPlay {
                        context_uri,
                        track_uri,
                        track_uid,
                        shuffle: shuffle.unwrap_or(current),
                    };
                    self.soloist(SoloistCmd::PlayContext(play))
                        .await
                        .map(|()| Reply::Ok)
                })
                .await
            }
            R::StartRadio { seed_uri } => {
                within(RADIO_CEILING, async {
                    let radio = self
                        .web
                        .radio_for(Lane::User, &seed_uri)
                        .await
                        .map_err(|e| self.failed("Couldn't start the radio", &e))?;
                    let shuffle = self.core.lock().await.state.player.shuffle;
                    let play = ConnectPlay {
                        context_uri: radio,
                        track_uri: None,
                        track_uid: None,
                        shuffle,
                    };
                    self.soloist(SoloistCmd::PlayContext(play))
                        .await
                        .map(|()| Reply::Ok)
                })
                .await
            }
            // The window shows its own heart's outcome; everyone else gets a notification.
            R::ToggleHeart { uri } => self.heart(uri, None, role == Role::Cli).await,
            R::SetLiked { uri, liked } => {
                self.heart(Some(uri), Some(liked), role == Role::Cli).await
            }
            R::LikedState { uris } => {
                let core = self.core.lock().await;
                Ok(Reply::LikedState(
                    uris.iter().map(|uri| core.is_liked(uri)).collect(),
                ))
            }
            R::Page { req, prefetch } => self.pages.get(req, prefetch, now_ms()).await,
            R::Lyrics { track_uri } => self
                .web
                .lyrics(Lane::User, &track_uri)
                .await
                .map(Reply::Lyrics)
                .map_err(|e| self.failed("Couldn't load the lyrics", &e)),
            // A track without credits (a 404) is an empty list: "No credits".
            R::Credits { track_uri } => self
                .web
                .credits(Lane::User, &track_uri)
                .await
                .map(Reply::Credits)
                .map_err(|e| self.failed("Couldn't load the credits", &e)),
            R::StartLogin => self
                .hints
                .send(SessionHint::StartLogin)
                .await
                .map(|()| Reply::Ok)
                .map_err(|_| SESSION_STOPPED.to_string()),
            R::SetSetting(setting) => self.set_setting(setting).await,
            R::ForgetSoloistKey => self.desktop.forget_soloist_key().await.map(|()| Reply::Ok),
            R::Quit => {
                log::info!("{role:?} client asked the engine to quit");
                close_windows_then(&**self.windows(), || self.request_quit()).await;
                Ok(Reply::Ok)
            }
            R::Play
            | R::Pause
            | R::Next
            | R::Prev
            | R::Seek { .. }
            | R::SetShuffle { .. }
            | R::SetRepeat { .. }
            | R::PlayUri { .. }
            | R::AddToQueue { .. }
            | R::RestartPlayer => unreachable!("player commands are handled above"),
        }
    }

    /// Answers requests from inside the engine (MPRIS, the tray) as a CLI client's are,
    /// each on its own task, until every sender is gone. A failure is logged: there is
    /// no one to tell.
    pub async fn on_local_requests(self: Arc<Self>, mut requests: mpsc::Receiver<Request>) {
        while let Some(req) = requests.recv().await {
            let engine = self.clone();
            tokio::spawn(async move {
                let what = format!("{req:?}");
                if let Err(e) = engine.handle(Role::Cli, req).await {
                    log::warn!("{what} (from the media controls or the tray): {e}");
                }
            });
        }
    }

    /// A setting: autostart goes through `systemctl` first, and changes nothing if that
    /// fails.
    async fn set_setting(self: &Arc<Self>, setting: Setting) -> Result<Reply, String> {
        if let Setting::Autostart(on) = setting {
            self.desktop.set_autostart(on).await?;
        }
        let mut core = self.core.lock().await;
        let effects = core.on_setting(setting);
        self.execute(&core, effects);
        Ok(Reply::Ok)
    }

    /// `<what>: <reason>` for a failed Spotify request, in a few words
    /// ([`crate::reason`]); the journal gets the whole error. It may also hint the session.
    fn failed(&self, what: &str, e: &SpotError) -> String {
        self.hint(e);
        log::warn!("{what}: {e}");
        format!("{what}: {}", crate::reason::failure_reason(e))
    }
}

/// `work`'s answer, or [`NO_ANSWER`] once `ceiling` has passed: bounded, so the window's
/// own wait (`engine_ipc::client::timeout_for`) can outlast it.
async fn within(
    ceiling: Duration,
    work: impl Future<Output = Result<Reply, String>>,
) -> Result<Reply, String> {
    tokio::time::timeout(ceiling, work)
        .await
        .unwrap_or_else(|_| {
            log::warn!("no answer in {} s; given up", ceiling.as_secs());
            Err(NO_ANSWER.into())
        })
}

/// Closes the open windows (waiting up to `CLOSE_WAIT`), then `quit`s: a window left
/// open would find the engine gone and start it again.
pub(crate) async fn close_windows_then(windows: &dyn WindowControl, quit: impl FnOnce()) {
    if windows.window_connected() && !windows.close_window(CLOSE_WAIT).await {
        log::warn!(
            "a window didn't close within {} ms; quitting anyway",
            CLOSE_WAIT.as_millis()
        );
    }
    quit();
}

/// The Soloist command a request is, for those that are only one.
pub fn player_command(req: &Request) -> Option<SoloistCmd> {
    use Request as R;
    use SoloistCmd as C;
    Some(match req {
        R::Play => C::Play(None),
        R::Pause => C::Pause,
        R::Next => C::Next,
        R::Prev => C::Prev,
        R::Seek { position_ms } => C::Seek(*position_ms),
        R::SetShuffle { on } => C::Shuffle(*on),
        R::SetRepeat { mode } => C::Repeat(*mode),
        R::PlayUri { uri } => C::Play(Some(uri.clone())),
        R::AddToQueue { uri } => C::Queue(uri.clone()),
        R::RestartPlayer => C::Restart,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use engine_ipc::proto::{Event, Settings};
    use playback::{command::Repeat, event::PlaybackEvent};

    use super::*;
    use crate::{
        core::{Core, OUTPUT_DEVICE_NOTICE},
        engine::testing::{TestEngine, settings, test_engine},
    };

    #[test]
    fn transport_requests_are_soloist_commands() {
        use Request as R;
        use SoloistCmd as C;
        let uri = "spotify:track:t".to_string();
        for (req, cmd) in [
            (R::Play, C::Play(None)),
            (R::Pause, C::Pause),
            (R::Next, C::Next),
            (R::Prev, C::Prev),
            (
                R::Seek {
                    position_ms: 61_000,
                },
                C::Seek(61_000),
            ),
            (R::SetShuffle { on: true }, C::Shuffle(true)),
            (
                R::SetRepeat {
                    mode: Repeat::Track,
                },
                C::Repeat(Repeat::Track),
            ),
            (R::PlayUri { uri: uri.clone() }, C::Play(Some(uri.clone()))),
            (R::AddToQueue { uri: uri.clone() }, C::Queue(uri.clone())),
            (R::RestartPlayer, C::Restart),
        ] {
            assert_eq!(player_command(&req), Some(cmd), "{req:?}");
        }
        for req in [
            R::SetVolume { volume: 1 },
            R::ToggleHeart { uri: None },
            R::SetLiked {
                uri: uri.clone(),
                liked: true,
            },
            R::Quit,
            R::StartLogin,
        ] {
            assert_eq!(player_command(&req), None, "{req:?}");
        }
    }

    /// A failed Spotify request reaches the window in a few words, never as the error's
    /// own text (which carries the request's path).
    #[tokio::test]
    async fn a_failed_spotify_request_says_why_plainly() {
        let t = test_engine(Core::new(settings())).await;
        let track_uri = "spotify:track:5kCNaSNJ6AebIX5cM991Tk".to_string();
        let lyrics = t
            .engine
            .handle(
                Role::Window,
                Request::Lyrics {
                    track_uri: track_uri.clone(),
                },
            )
            .await;
        assert_eq!(
            lyrics.unwrap_err(),
            "Couldn't load the lyrics: not logged in to Spotify"
        );
        let credits = t
            .engine
            .handle(Role::Window, Request::Credits { track_uri })
            .await;
        assert_eq!(
            credits.unwrap_err(),
            "Couldn't load the credits: not logged in to Spotify"
        );
    }

    /// Answers the next Soloist command `Ok` and returns it.
    async fn answer(t: &mut TestEngine) -> SoloistCmd {
        let (cmd, reply) = t.soloist.recv().await.expect("a Soloist command");
        reply.send(Ok(())).unwrap();
        cmd
    }

    /// Final review m2: what a window waits for is bounded in the engine, and the
    /// client's own timeouts outlast each bound, so "didn't answer in time" never shows
    /// for an action that lands afterwards.
    #[test]
    fn client_timeouts_outlast_the_engine() {
        use engine_ipc::client::timeout_for;
        let uri = || "spotify:track:t".to_string();
        let play = Request::PlayContext {
            context_uri: "spotify:playlist:p".into(),
            track_uri: Some(uri()),
            track_uid: None,
            shuffle: None,
        };
        assert!(timeout_for(&play) > PLAY_CEILING);
        let radio = Request::StartRadio { seed_uri: uri() };
        assert!(timeout_for(&radio) > RADIO_CEILING);
        let presses = crate::heart::TURN_CEILING + crate::heart::WRITE_CEILING;
        for heart in [
            Request::ToggleHeart { uri: None },
            Request::SetLiked {
                uri: uri(),
                liked: true,
            },
        ] {
            assert!(timeout_for(&heart) > presses, "{heart:?}");
        }
        let page = Request::Page {
            req: engine_ipc::PageRequest::Home,
            prefetch: false,
        };
        assert!(timeout_for(&page) > crate::pages::UNCACHED_LIMIT);
    }

    /// Final review m2: a context play Soloist (or Spotify) never answers is answered at
    /// `PLAY_CEILING`, inside the window's wait.
    #[tokio::test(start_paused = true)]
    async fn a_play_nobody_answers_ends_at_its_ceiling() {
        let t = test_engine(Core::new(settings())).await;
        let req = Request::PlayContext {
            context_uri: "spotify:playlist:p".into(),
            track_uri: None,
            track_uid: None,
            shuffle: None,
        };
        let started = tokio::time::Instant::now();
        let reply = t.engine.handle(Role::Window, req).await;
        assert_eq!(reply, Err(NO_ANSWER.into()));
        assert_eq!(started.elapsed(), PLAY_CEILING);
        drop(t);
    }

    #[tokio::test]
    async fn play_context_without_shuffle_keeps_the_players() {
        let mut core = Core::new(settings());
        core.on_playback(&PlaybackEvent::Options {
            shuffle: true,
            repeat: Repeat::Off,
        });
        let mut t = test_engine(core).await;
        let engine = t.engine.clone();
        let req = Request::PlayContext {
            context_uri: "spotify:playlist:p".into(),
            track_uri: Some("spotify:track:t".into()),
            track_uid: Some("u1".into()),
            shuffle: None,
        };
        let handled = tokio::spawn(async move { engine.handle(Role::Window, req).await });
        assert_eq!(
            answer(&mut t).await,
            SoloistCmd::PlayContext(ConnectPlay {
                context_uri: "spotify:playlist:p".into(),
                track_uri: Some("spotify:track:t".into()),
                track_uid: Some("u1".into()),
                shuffle: true,
            })
        );
        assert_eq!(handled.await.unwrap(), Ok(Reply::Ok));
    }

    #[tokio::test]
    async fn volume_is_saved_broadcast_and_sent() {
        let mut t = test_engine(Core::new(settings())).await;
        let (_, mut events) = t.engine.welcome().await;
        let engine = t.engine.clone();
        let handled = tokio::spawn(async move {
            engine
                .handle(Role::Window, Request::SetVolume { volume: 140 })
                .await
        });
        assert_eq!(answer(&mut t).await, SoloistCmd::Volume(100));
        assert_eq!(handled.await.unwrap(), Ok(Reply::Ok));
        assert_eq!(
            events.recv().await.unwrap(),
            Event::Settings(Settings {
                volume: 100,
                ..settings()
            })
        );
        t.engine.flush_settings().await;
        let saved = crate::settings::load(&t.dir.path().join("settings.json"), settings()).await;
        assert_eq!(saved.volume, 100);
    }

    /// Final review C1: before Liked Songs has been read, a heart the engine hasn't
    /// checked is unknown, not "not liked".
    #[tokio::test]
    async fn liked_state_comes_from_the_liked_set() {
        let ask = || Request::LikedState {
            uris: vec!["spotify:track:b".into(), "spotify:track:a".into()],
        };
        let t = test_engine(Core::new(settings())).await;
        let reply = t.engine.handle(Role::Window, ask()).await;
        assert_eq!(reply, Ok(Reply::LikedState(vec![None, None])));

        t.engine
            .core
            .lock()
            .await
            .set_liked_set(["spotify:track:a".to_string()]);
        let reply = t.engine.handle(Role::Window, ask()).await;
        assert_eq!(reply, Ok(Reply::LikedState(vec![Some(false), Some(true)])));
    }

    /// Final review C1: the window's heart writes the state it asks for, even one the
    /// engine thinks the track has already; only the key, the tray and the CLI are told
    /// by notification.
    #[tokio::test]
    async fn set_liked_writes_what_the_window_asks() {
        let mut core = Core::new(settings());
        core.set_liked_set(["spotify:track:a".to_string()]);
        let t = test_engine(core).await;
        let set = |liked| Request::SetLiked {
            uri: "spotify:track:a".into(),
            liked,
        };
        let reply = t.engine.handle(Role::Window, set(true)).await;
        assert_eq!(
            reply,
            Ok(Reply::Liked {
                uri: "spotify:track:a".into(),
                liked: true
            })
        );
        assert!(t.desktop.notified().is_empty());
        let reply = t.engine.handle(Role::Cli, set(false)).await;
        assert_eq!(
            reply,
            Ok(Reply::Liked {
                uri: "spotify:track:a".into(),
                liked: false
            })
        );
        assert_eq!(
            t.desktop.notified(),
            ["Removed from Liked Songs: spotify:track:a"]
        );
        assert_eq!(
            *t.liker.calls.lock().unwrap(),
            [
                ("spotify:track:a".to_string(), true),
                ("spotify:track:a".to_string(), false)
            ]
        );
    }

    #[tokio::test]
    async fn settings_requests() {
        let t = test_engine(Core::new(settings())).await;
        let (_, mut events) = t.engine.welcome().await;
        let reply = t
            .engine
            .handle(Role::Window, Request::SetSetting(Setting::Autostart(true)))
            .await;
        assert_eq!(reply, Ok(Reply::Ok));
        assert_eq!(*t.desktop.autostart.lock().unwrap(), [true]);
        assert!(t.engine.core.lock().await.state.settings.autostart);
        assert!(matches!(events.recv().await.unwrap(), Event::Settings(s) if s.autostart));

        let device = Setting::OutputDevice(Some("easyeffects_sink".into()));
        let reply = t
            .engine
            .handle(Role::Window, Request::SetSetting(device))
            .await;
        assert_eq!(reply, Ok(Reply::Ok));
        assert!(matches!(events.recv().await.unwrap(), Event::Settings(_)));
        assert!(matches!(
            events.recv().await.unwrap(),
            Event::Notice(n) if n.text == OUTPUT_DEVICE_NOTICE
        ));

        let reply = t
            .engine
            .handle(Role::Window, Request::ForgetSoloistKey)
            .await;
        assert_eq!(reply, Ok(Reply::Ok));
        assert_eq!(t.desktop.forgot.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn start_login_hints_the_session_and_quit_is_requested() {
        let mut t = test_engine(Core::new(settings())).await;
        let reply = t.engine.handle(Role::Window, Request::StartLogin).await;
        assert_eq!(reply, Ok(Reply::Ok));
        assert_eq!(t.hints.try_recv(), Ok(SessionHint::StartLogin));

        let quit = tokio::spawn({
            let engine = t.engine.clone();
            async move { engine.quit_requested().await }
        });
        let reply = t.engine.handle(Role::Cli, Request::Quit).await;
        assert_eq!(reply, Ok(Reply::Ok));
        quit.await.unwrap();
    }

    /// Windows that record their close in `log`, and close (or not) as told.
    struct LoggedWindows {
        connected: bool,
        closes: bool,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl WindowControl for LoggedWindows {
        fn window_connected(&self) -> bool {
            self.connected
        }
        async fn close_window(&self, wait: std::time::Duration) -> bool {
            self.log
                .lock()
                .unwrap()
                .push(format!("close {} ms", wait.as_millis()));
            self.closes
        }
    }

    #[tokio::test]
    async fn quit_closes_the_windows_first() {
        for (connected, closes, expected) in [
            (true, true, &["close 1500 ms", "quit"][..]),
            // One that won't close doesn't stop the quit.
            (true, false, &["close 1500 ms", "quit"][..]),
            // No window: nothing to close.
            (false, true, &["quit"][..]),
        ] {
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            let windows = LoggedWindows {
                connected,
                closes,
                log: log.clone(),
            };
            close_windows_then(&windows, || log.lock().unwrap().push("quit".into())).await;
            assert_eq!(*log.lock().unwrap(), expected, "{connected} {closes}");
        }
    }

    #[tokio::test]
    async fn player_commands_after_quit_say_so() {
        let t = test_engine(Core::new(settings())).await;
        t.engine.close_soloist();
        let reply = t.engine.handle(Role::Window, Request::Pause).await;
        assert_eq!(reply, Err(crate::engine::QUITTING.into()));
    }
}
