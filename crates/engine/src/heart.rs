//! The heart: like or unlike one track, the only write LOUDAUTO makes to the account.
//! The global key, the tray, `loudauto-cli heart` and the window all come here. The
//! window's hearts, and the tray's while it knows the track's state, name the state they
//! want (`Request::SetLiked`); the others toggle (`Request::ToggleHeart`), which, for a
//! track whose state the engine doesn't know yet, asks Spotify first rather than guess.
//! With no uri a press acts on the current track, which is the phone's when another
//! device is playing, since Soloist mirrors that device's playback.

use std::time::Duration;

use async_trait::async_trait;
use spot_core::{
    SpotError,
    web::{Web, lanes::Lane},
};
use tokio::{
    sync::Mutex,
    time::{Instant, timeout, timeout_at},
};

use crate::{
    core::{Core, Effect},
    notify,
};

/// Reads and writes one track's liked state on the account (`Web::in_library`,
/// `Web::set_liked`); a trait so tests never touch a real account.
#[async_trait]
pub trait Liker: Send + Sync {
    /// Whether `uri` is in Liked Songs now.
    async fn is_liked(&self, uri: &str) -> spot_core::Result<bool>;
    async fn set_liked(&self, uri: &str, liked: bool) -> spot_core::Result<()>;
}

#[async_trait]
impl Liker for Web {
    async fn is_liked(&self, uri: &str) -> spot_core::Result<bool> {
        let answers = self.in_library(Lane::User, &[uri.to_owned()]).await?;
        answers.first().copied().ok_or_else(|| {
            SpotError::UnexpectedPage(format!("areEntitiesInLibrary (no answer for {uri})"))
        })
    }

    async fn set_liked(&self, uri: &str, liked: bool) -> spot_core::Result<()> {
        Web::set_liked(self, uri, liked).await
    }
}

/// A heart press that reached the account.
#[derive(Debug, Clone, PartialEq)]
pub struct HeartOutcome {
    pub uri: String,
    /// The track's liked state now.
    pub liked: bool,
    /// What to tell the owner: `Liked: …` or `Removed from Liked Songs: …`.
    pub message: String,
    /// What `Core::on_liked` returned; the caller executes them.
    pub effects: Vec<Effect>,
}

/// How long a press waits for the presses before it: past one press's own
/// [`WRITE_CEILING`], so the press after one still gets its turn (and undoes it, as the
/// key promises), while a pile of presses can't make the last wait without end.
pub const TURN_CEILING: Duration = Duration::from_secs(25);

/// The longest a press waits for Spotify, reading the track's state when it isn't known
/// and then writing: past one request's 15 s timeout, so a slow answer still lands, and
/// short enough that every press gets an answer.
pub const WRITE_CEILING: Duration = Duration::from_secs(20);

/// Why a press that waited [`TURN_CEILING`] for its turn wrote nothing.
const STILL_BUSY: &str = "another heart press is still waiting for Spotify";
/// Why a press Spotify didn't answer within [`WRITE_CEILING`] failed.
const NO_ANSWER: &str = "Spotify didn't answer";

/// Heart presses, one at a time: a second press waits for the first one's write, so a
/// toggle undoes it instead of sending the same write again. One per engine.
#[derive(Default)]
pub struct Presses(Mutex<()>);

impl Presses {
    /// Toggles `uri`, or the current track when `None` (the phone's, when the phone
    /// plays): the key, the CLI, and the tray while the track's state isn't known. A
    /// track whose state isn't known yet is read from Spotify first. `Err` is the whole
    /// `Couldn't like: …` message.
    pub async fn toggle(
        &self,
        core: &Mutex<Core>,
        liker: &dyn Liker,
        uri: Option<String>,
    ) -> Result<HeartOutcome, String> {
        self.press(core, liker, uri, None).await
    }

    /// Makes `uri` liked or not, as the window's heart (or the tray's) asked, whatever
    /// the engine thinks it is now.
    pub async fn set(
        &self,
        core: &Mutex<Core>,
        liker: &dyn Liker,
        uri: String,
        liked: bool,
    ) -> Result<HeartOutcome, String> {
        self.press(core, liker, Some(uri), Some(liked)).await
    }

    /// One press: `want`, else the opposite of the track's state. The state is read,
    /// then let go while Spotify is asked, then updated once Spotify has taken the
    /// write; a failed write changes nothing.
    async fn press(
        &self,
        core: &Mutex<Core>,
        liker: &dyn Liker,
        uri: Option<String>,
        want: Option<bool>,
    ) -> Result<HeartOutcome, String> {
        let Ok(_turn) = timeout(TURN_CEILING, self.0.lock()).await else {
            log::warn!(
                "heart: a press waited {} s for the ones before it; dropped",
                TURN_CEILING.as_secs()
            );
            return Err(heart_failure(STILL_BUSY));
        };
        let target = target(&*core.lock().await, uri)?;
        let deadline = Instant::now() + WRITE_CEILING;
        let liked = match want.or(target.now.map(|now| !now)) {
            Some(liked) => liked,
            // A toggle of a track the engine doesn't know yet: ask, never guess.
            None => match timeout_at(deadline, liker.is_liked(&target.uri)).await {
                Ok(Ok(now)) => !now,
                Ok(Err(e)) => {
                    log::warn!("heart: couldn't read {}: {e}", target.uri);
                    return Err(heart_failure(&failure_reason(&e)));
                }
                Err(_) => {
                    log::warn!("heart: reading {}: no answer in time", target.uri);
                    return Err(heart_failure(NO_ANSWER));
                }
            },
        };
        let verb = if liked { "like" } else { "unlike" };
        let failed = match timeout_at(deadline, liker.set_liked(&target.uri, liked)).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => {
                log::warn!("heart: {verb} {} failed: {e}", target.uri);
                Some(failure_reason(&e))
            }
            Err(_) => {
                log::warn!(
                    "heart: {verb} {}: no answer in {} s",
                    target.uri,
                    WRITE_CEILING.as_secs()
                );
                Some(NO_ANSWER.to_string())
            }
        };
        if let Some(reason) = failed {
            return Err(heart_failure(&reason));
        }
        log::info!("heart: {verb}d {}", target.uri);
        let effects = core.lock().await.on_liked(&target.uri, liked);
        Ok(HeartOutcome {
            message: heart_message(&target.title, &target.artists, liked),
            uri: target.uri,
            liked,
            effects,
        })
    }
}

/// `Liked: <title> — <artist, artist>` or `Removed from Liked Songs: …`; just the title
/// when there are no artists.
pub fn heart_message(title: &str, artists: &[String], liked: bool) -> String {
    let what = if liked {
        "Liked"
    } else {
        "Removed from Liked Songs"
    };
    if artists.is_empty() {
        format!("{what}: {title}")
    } else {
        format!("{what}: {title} — {}", artists.join(", "))
    }
}

/// `Couldn't like: <reason>`.
pub fn heart_failure(reason: &str) -> String {
    format!("Couldn't like: {reason}")
}

/// The text a press's notification shows: its message, or why it failed.
pub fn announcement(result: &Result<HeartOutcome, String>) -> &str {
    match result {
        Ok(outcome) => &outcome.message,
        Err(message) => message,
    }
}

/// Shows a press's answer as a desktop notification. For the key, the tray and the CLI;
/// not for the window's heart button, since the window shows it.
pub fn announce(result: &Result<HeartOutcome, String>) {
    notify::show(announcement(result), "");
}

/// What a press acts on.
struct Target {
    uri: String,
    title: String,
    artists: Vec<String>,
    /// Whether it is liked now, as far as the engine knows.
    now: Option<bool>,
}

/// `uri`, or the current track, and its state as known. A uri that isn't the current
/// track is named by its uri: its title isn't known here.
fn target(core: &Core, uri: Option<String>) -> Result<Target, String> {
    let current = core.state.player.track.as_ref();
    let (uri, title, artists) = match (uri, current) {
        (None, None) => return Err(heart_failure("nothing is playing")),
        (None, Some(track)) => (
            track.uri.clone(),
            track.title.clone(),
            track.artists.clone(),
        ),
        (Some(uri), Some(track)) if track.uri == uri => {
            (uri, track.title.clone(), track.artists.clone())
        }
        (Some(uri), _) => (uri.clone(), uri, Vec::new()),
    };
    Ok(Target {
        now: core.is_liked(&uri),
        uri,
        title,
        artists,
    })
}

/// A short reason for the owner ([`crate::reason`]), in the heart's own words where a
/// like differs from any other request.
fn failure_reason(e: &SpotError) -> String {
    match e {
        // `Web::set_liked` refuses anything but a track.
        SpotError::InvalidUri(_) => "only tracks can be liked".into(),
        SpotError::GraphQl(_) => "Spotify refused the change".into(),
        e => crate::reason::failure_reason(e),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use engine_ipc::proto::{Event, Settings};
    use playback::event::{NowPlaying, PlayStatus, PlaybackEvent};
    use tokio::sync::Semaphore;

    use super::*;

    const T: &str = "spotify:track:1XD4K4CGAKTIBmFpvuaFru";
    const U: &str = "spotify:track:4SWyOAtHVNdnczEIsLvMN2";

    /// How the fake account answers a write.
    enum Answer {
        Ok,
        Fail(fn() -> SpotError),
        /// Never answers.
        Hang,
        /// Answers `Ok` once the test adds a permit.
        Gate(Semaphore),
    }

    /// Records each `set_liked(uri, liked)` and answers as told; a read of the account
    /// says `on_account` (`None`: the read fails with a 503).
    struct Fake {
        answer: Answer,
        on_account: Option<bool>,
        calls: StdMutex<Vec<(String, bool)>>,
        reads: StdMutex<Vec<String>>,
    }

    impl Fake {
        fn new(answer: Answer) -> Self {
            Fake {
                answer,
                on_account: Some(false),
                calls: StdMutex::new(Vec::new()),
                reads: StdMutex::new(Vec::new()),
            }
        }

        /// What a read of the account answers.
        fn reading(self, on_account: Option<bool>) -> Self {
            Fake { on_account, ..self }
        }

        fn calls(&self) -> Vec<(String, bool)> {
            self.calls.lock().unwrap().clone()
        }

        fn reads(&self) -> Vec<String> {
            self.reads.lock().unwrap().clone()
        }
    }

    /// A press of its own: one at a time only among presses that share their `Presses`.
    async fn toggle(
        core: &Mutex<Core>,
        liker: &dyn Liker,
        uri: Option<String>,
    ) -> Result<HeartOutcome, String> {
        Presses::default().toggle(core, liker, uri).await
    }

    #[async_trait]
    impl Liker for Fake {
        async fn is_liked(&self, uri: &str) -> spot_core::Result<bool> {
            self.reads.lock().unwrap().push(uri.into());
            self.on_account.ok_or(SpotError::Http {
                status: 503,
                what: "/pathfinder/v2/query".into(),
            })
        }

        async fn set_liked(&self, uri: &str, liked: bool) -> spot_core::Result<()> {
            self.calls.lock().unwrap().push((uri.into(), liked));
            match &self.answer {
                Answer::Ok => Ok(()),
                Answer::Fail(error) => Err(error()),
                Answer::Hang => std::future::pending().await,
                Answer::Gate(gate) => {
                    gate.acquire().await.expect("gate open").forget();
                    Ok(())
                }
            }
        }
    }

    fn settings() -> Settings {
        Settings {
            volume: 40,
            output_device: None,
            autostart: false,
        }
    }

    fn nerve() -> NowPlaying {
        NowPlaying {
            uri: T.into(),
            title: "nerve".into(),
            artists: vec!["2hollis".into()],
            artist_uris: vec!["spotify:artist:2rLGlNI6htigNxx172qxLu".into()],
            album: Some("boy".into()),
            album_uri: Some("spotify:album:21b4cDNse2AMpj94ykfuON".into()),
            duration_ms: Some(150_000),
            cover_url: None,
        }
    }

    /// The phone plays nerve: Soloist mirrors its track with `is_active: false`.
    fn phone_playing() -> Mutex<Core> {
        let mut core = Core::new(settings());
        core.on_playback(&PlaybackEvent::Active(false));
        core.on_playback(&PlaybackEvent::Status(PlayStatus::Playing));
        core.on_playback(&PlaybackEvent::Track(nerve()));
        assert!(!core.state.player.active_here);
        Mutex::new(core)
    }

    fn liked_changed(effects: &[Effect], uri: &str, liked: bool) -> bool {
        effects.contains(&Effect::Broadcast(Event::LikedChanged {
            uri: uri.into(),
            liked,
        }))
    }

    fn calls(list: &[(&str, bool)]) -> Vec<(String, bool)> {
        list.iter().map(|(u, l)| (u.to_string(), *l)).collect()
    }

    #[tokio::test]
    async fn likes_phone_track_when_remote() {
        let core = phone_playing();
        let liker = Fake::new(Answer::Ok);

        let outcome = toggle(&core, &liker, None).await.unwrap();

        assert_eq!(liker.calls(), calls(&[(T, true)]));
        assert_eq!(outcome.uri, T);
        assert!(outcome.liked);
        assert_eq!(outcome.message, "Liked: nerve — 2hollis");
        assert!(liked_changed(&outcome.effects, T, true));
        assert!(
            outcome
                .effects
                .iter()
                .any(|e| matches!(e, Effect::Broadcast(Event::Player(p)) if p.liked == Some(true))),
            "the bottom bar's heart fills"
        );
        {
            let core = core.lock().await;
            assert_eq!(core.is_liked(T), Some(true));
            assert_eq!(core.state.player.liked, Some(true));
        }
        // The key's notification names the phone's track.
        assert_eq!(announcement(&Ok(outcome)), "Liked: nerve — 2hollis");
    }

    #[tokio::test]
    async fn second_press_unlikes() {
        let core = phone_playing();
        let liker = Fake::new(Answer::Ok);

        toggle(&core, &liker, None).await.unwrap();
        let second = toggle(&core, &liker, None).await.unwrap();

        assert!(!second.liked);
        assert_eq!(second.message, "Removed from Liked Songs: nerve — 2hollis");
        assert!(liked_changed(&second.effects, T, false));
        assert_eq!(liker.calls(), calls(&[(T, true), (T, false)]));
        let core = core.lock().await;
        assert_eq!(core.is_liked(T), Some(false));
        assert_eq!(core.state.player.liked, Some(false));
    }

    #[tokio::test]
    async fn nothing_playing_message() {
        let core = Mutex::new(Core::new(settings()));
        let liker = Fake::new(Answer::Ok);

        let result = toggle(&core, &liker, None).await;

        assert_eq!(result, Err("Couldn't like: nothing is playing".to_string()));
        assert_eq!(announcement(&result), "Couldn't like: nothing is playing");
        assert_eq!(liker.calls(), [], "nothing is written");
    }

    #[tokio::test]
    async fn failure_reverts_and_explains() {
        let fail = || SpotError::Http {
            status: 500,
            what: "/pathfinder/v2/query".into(),
        };

        // A like that fails leaves the track unliked.
        let core = phone_playing();
        core.lock().await.set_liked_set([]);
        let err = toggle(&core, &Fake::new(Answer::Fail(fail)), None)
            .await
            .unwrap_err();
        assert!(err.starts_with("Couldn't like:"), "{err}");
        assert_eq!(err, "Couldn't like: Spotify answered 500");
        {
            let core = core.lock().await;
            assert_eq!(core.is_liked(T), Some(false));
            assert_eq!(core.state.player.liked, Some(false));
        }

        // An unlike that fails leaves it liked.
        core.lock().await.set_liked_set([T.to_string()]);
        let err = toggle(&core, &Fake::new(Answer::Fail(fail)), None)
            .await
            .unwrap_err();
        assert_eq!(err, "Couldn't like: Spotify answered 500");
        let core = core.lock().await;
        assert_eq!(core.is_liked(T), Some(true));
        assert_eq!(core.state.player.liked, Some(true));
    }

    /// The message for a press that fails with `error`.
    async fn failure_text(error: fn() -> SpotError) -> String {
        let core = phone_playing();
        toggle(&core, &Fake::new(Answer::Fail(error)), None)
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn heart_failure_reasons_are_short_and_clean() {
        let refused = || {
            SpotError::Net(
                "/pathfinder/v2/query: error sending request: client error (Connect)".into(),
            )
        };
        assert_eq!(
            failure_text(refused).await,
            "Couldn't like: no connection to Spotify"
        );
        let episode = || SpotError::InvalidUri("spotify:episode:x isn't a track".into());
        assert_eq!(
            failure_text(episode).await,
            "Couldn't like: only tracks can be liked"
        );
        let reshaped = || SpotError::UnexpectedPage("addToLibrary (no data)".into());
        assert_eq!(
            failure_text(reshaped).await,
            "Couldn't like: Spotify's answer didn't make sense"
        );
        assert_eq!(
            failure_text(|| SpotError::NotLoggedIn).await,
            "Couldn't like: not logged in to Spotify"
        );
        let graphql = || SpotError::GraphQl("Something went wrong at addLibraryItems".into());
        assert_eq!(
            failure_text(graphql).await,
            "Couldn't like: Spotify refused the change"
        );
    }

    #[test]
    fn heart_message_copy() {
        assert_eq!(
            heart_message("nerve", &["2hollis".into()], true),
            "Liked: nerve — 2hollis"
        );
        assert_eq!(
            heart_message("tell me", &["2hollis".into(), "brakence".into()], false),
            "Removed from Liked Songs: tell me — 2hollis, brakence"
        );
        assert_eq!(heart_message("nerve", &[], true), "Liked: nerve");
        assert_eq!(
            heart_failure("nothing is playing"),
            "Couldn't like: nothing is playing"
        );
    }

    #[tokio::test]
    async fn heart_by_uri_from_a_row() {
        let core = phone_playing();
        let liker = Fake::new(Answer::Ok);

        // A row that isn't playing: only its uri is known here.
        let row = toggle(&core, &liker, Some(U.into())).await.unwrap();
        assert!(row.liked);
        assert_eq!(row.message, format!("Liked: {U}"));
        assert!(liked_changed(&row.effects, U, true));
        assert_eq!(
            core.lock().await.state.player.liked,
            None,
            "the playing track is untouched (and still unknown)"
        );

        // The playing track's row is named.
        let playing = toggle(&core, &liker, Some(T.into())).await.unwrap();
        assert_eq!(playing.message, "Liked: nerve — 2hollis");
        assert_eq!(liker.calls(), calls(&[(U, true), (T, true)]));
    }

    #[tokio::test]
    async fn heart_presses_take_turns() {
        let core = Arc::new(phone_playing());
        let liker = Arc::new(Fake::new(Answer::Gate(Semaphore::new(0))));
        let presses = Arc::new(Presses::default());
        let press = || {
            let (core, liker, presses) = (core.clone(), liker.clone(), presses.clone());
            tokio::spawn(async move { presses.toggle(&core, &*liker, None).await })
        };

        let first = press();
        tokio::time::timeout(Duration::from_secs(5), async {
            while liker.calls().is_empty() && !first.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first press reaches Spotify");
        assert!(!first.is_finished(), "the first press waits for Spotify");
        assert!(
            core.try_lock().is_ok(),
            "the engine's state is free while the write is out"
        );
        let second = press();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            liker.calls().len(),
            1,
            "the second press waits for the first"
        );

        let Answer::Gate(gate) = &liker.answer else {
            unreachable!()
        };
        gate.add_permits(2);
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();

        // So a quick second press undoes the first, as the key promises.
        assert!(first.liked);
        assert!(!second.liked);
        assert_eq!(liker.calls(), calls(&[(T, true), (T, false)]));
        assert_eq!(core.lock().await.is_liked(T), Some(false));
    }

    /// Final review C1: the window's heart names the state it wants, and that is what is
    /// written, whatever the engine thinks the track is now: a heart the window drew as
    /// not liked (unknown, or stale) likes, it never unlikes.
    #[tokio::test]
    async fn set_writes_the_state_asked_for() {
        let core = phone_playing();
        core.lock().await.set_liked_set([T.to_string()]);
        let liker = Fake::new(Answer::Ok);
        let presses = Presses::default();

        let liked = presses.set(&core, &liker, T.into(), true).await.unwrap();
        assert!(liked.liked);
        assert_eq!(liked.message, "Liked: nerve — 2hollis");
        assert!(liked_changed(&liked.effects, T, true));
        let unliked = presses.set(&core, &liker, T.into(), false).await.unwrap();
        assert!(!unliked.liked);
        assert_eq!(liker.calls(), calls(&[(T, true), (T, false)]));
        assert_eq!(liker.reads(), Vec::<String>::new(), "nothing to read");
        assert_eq!(core.lock().await.is_liked(T), Some(false));
    }

    /// Final review C1: a toggle (the key, the tray, the CLI) of a track whose heart the
    /// engine doesn't know yet asks Spotify first, so the key can unlike before Liked
    /// Songs has been read; a read that fails writes nothing.
    #[tokio::test]
    async fn toggle_reads_an_unknown_heart_first() {
        let core = phone_playing();
        assert_eq!(core.lock().await.is_liked(T), None);
        let liker = Fake::new(Answer::Ok).reading(Some(true));
        let outcome = toggle(&core, &liker, None).await.unwrap();
        assert_eq!(liker.reads(), [T]);
        assert_eq!(liker.calls(), calls(&[(T, false)]));
        assert_eq!(outcome.message, "Removed from Liked Songs: nerve — 2hollis");
        assert_eq!(core.lock().await.is_liked(T), Some(false));

        let core = phone_playing();
        let liker = Fake::new(Answer::Ok).reading(None);
        let err = toggle(&core, &liker, None).await.unwrap_err();
        assert_eq!(err, "Couldn't like: Spotify answered 503");
        assert_eq!(liker.calls(), [], "nothing is written");
        assert_eq!(core.lock().await.is_liked(T), None);
    }

    /// Final review m2: a press waits for the ones before it at most `TURN_CEILING`, so
    /// a pile of presses can't outwait the client; the second still gets its turn.
    #[tokio::test(start_paused = true)]
    async fn a_press_waits_its_turn_at_most_the_turn_ceiling() {
        assert!(
            TURN_CEILING > WRITE_CEILING,
            "the press after one gets its turn"
        );
        let core = Arc::new(phone_playing());
        let presses = Arc::new(Presses::default());
        let hung = Arc::new(Fake::new(Answer::Hang));
        let started = tokio::time::Instant::now();
        let press = |liker: Arc<Fake>| {
            let (core, presses) = (core.clone(), presses.clone());
            tokio::spawn(async move {
                let result = presses.toggle(&core, &*liker, None).await;
                (result, started.elapsed())
            })
        };
        let first = press(hung.clone());
        while hung.calls().is_empty() {
            tokio::task::yield_now().await;
        }
        let second = press(hung.clone());
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let ok = Arc::new(Fake::new(Answer::Ok));
        let third = press(ok.clone());

        let (third, waited) = third.await.unwrap();
        assert_eq!(
            third,
            Err("Couldn't like: another heart press is still waiting for Spotify".into())
        );
        assert_eq!(waited, TURN_CEILING);
        assert_eq!(ok.calls(), [], "nothing is written");
        let (first, _) = first.await.unwrap();
        assert_eq!(first, Err("Couldn't like: Spotify didn't answer".into()));
        let (second, took) = second.await.unwrap();
        assert_eq!(second, Err("Couldn't like: Spotify didn't answer".into()));
        assert_eq!(took, WRITE_CEILING * 2, "the second had its turn");
        assert_eq!(hung.calls().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn heart_answers_when_spotify_never_does() {
        let core = phone_playing();

        // Time is paused, so the ceiling passes at once.
        let err = toggle(&core, &Fake::new(Answer::Hang), None)
            .await
            .unwrap_err();
        assert_eq!(err, "Couldn't like: Spotify didn't answer");
        assert_eq!(core.lock().await.is_liked(T), None, "nothing learnt");

        // The hung press doesn't block the next one.
        let next = toggle(&core, &Fake::new(Answer::Ok), None).await.unwrap();
        assert!(next.liked);
    }
}
