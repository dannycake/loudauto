//! The running engine: the state every client draws from (`Core`, behind one lock), the
//! handles to Spotify, Soloist and the session, and the loops that fold what they report
//! into the state. `run` builds one and the server answers clients from it.
//!
//! - Soloist's events go through `Core::on_playback`; the effects run at once, still under
//!   the core lock, and never wait for Soloist (its replies come on other tasks).
//! - Session updates fill `session`, `login` and `username`; Soloist's health fills
//!   `soloist`; both are broadcast. Soloist failing, expiring or going missing is also a
//!   desktop notification while no window is open.
//! - The liked set is filled from Liked Songs once the session is ready, and refreshed
//!   every 10 minutes. A refresh a heart press overlaps is dropped, so it can't undo it.
//!   A first read that fails (or is dropped) is tried again after 5, 15 and 60 s: until
//!   it lands, every heart the engine hasn't checked is unknown.
//! - The pathfinder hashes are refreshed hourly when due, the first time shortly after
//!   start.
//! - A request that fails on the session hints the session supervisor (`hinting`).

use std::{
    sync::{
        Arc, Mutex as StdMutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use engine_ipc::proto::{
    EngineState, Event, Level, PlayerState, Reply, SessionHealth, SoloistHealth,
};
use log::{debug, info, warn};
use playback::{
    error::{DOWNLOADS_URL, UPDATE_COMMAND},
    event::PlaybackEvent,
};
use spot_core::{
    SpotError,
    library::Library,
    web::{Web, lanes::Lane},
};
use tokio::{
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
    time::{MissedTickBehavior, interval_at, sleep},
};

use crate::{
    core::{Core, Effect},
    desktop::Desktop,
    heart::{self, Liker, Presses},
    hinting::send_hint,
    pages::PageService,
    server::{WindowControl, Windows},
    session::{SessionHint, SessionUpdate},
    settings::Saver,
    soloist::{CmdReply, SoloistCmd},
};

/// How often Liked Songs is read again, for hearts changed on other devices.
pub const LIKED_EVERY: Duration = Duration::from_secs(10 * 60);
/// After a first read of Liked Songs that failed, or that a heart press overlapped, the
/// waits before the next tries; after them, every [`LIKED_EVERY`].
pub const FIRST_READ_RETRIES: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];
/// The first hash refresh, after the start has settled.
pub const HASHES_FIRST_AFTER: Duration = Duration::from_secs(30);
/// How often the hashes are checked; `refresh_hashes_if_due` scrapes only once a day.
pub const HASHES_EVERY: Duration = Duration::from_secs(60 * 60);
/// The answer to a player command once the engine is quitting.
pub const QUITTING: &str = "LOUDAUTO is quitting.";
/// The answer to a player command Soloist's loop dropped unanswered.
pub const PLAYER_STOPPED: &str = "The player stopped.";

/// The channel to the Soloist loop.
pub type SoloistTx = mpsc::Sender<(SoloistCmd, CmdReply)>;

/// Where the liked set comes from: the library's Liked Songs (`Library::liked`); a trait
/// so tests can script it.
#[async_trait]
pub trait LikedSongs: Send + Sync {
    /// Every track in Liked Songs, read on `lane`.
    async fn liked_uris(&self, lane: Lane) -> spot_core::Result<Vec<String>>;
}

#[async_trait]
impl LikedSongs for Library {
    async fn liked_uris(&self, lane: Lane) -> spot_core::Result<Vec<String>> {
        let entries = self.liked(lane).await?;
        Ok(entries.into_iter().map(|entry| entry.uri).collect())
    }
}

/// What an `Engine` is built from.
pub struct Parts {
    pub core: Core,
    /// Every client's events; `PageService` sends on it too.
    pub events: broadcast::Sender<Event>,
    pub pages: PageService,
    pub web: Arc<Web>,
    /// Liked Songs, for the liked set.
    pub library: Arc<dyn LikedSongs>,
    /// Heart writes (`Web`, hinting the session).
    pub liker: Arc<dyn Liker>,
    pub soloist: SoloistTx,
    pub hints: mpsc::Sender<SessionHint>,
    pub saver: Saver,
    pub desktop: Arc<dyn Desktop>,
}

pub struct Engine {
    pub(crate) core: Mutex<Core>,
    events: broadcast::Sender<Event>,
    /// The player as last broadcast, for followers inside the engine (MPRIS).
    player: watch::Sender<PlayerState>,
    pub(crate) pages: PageService,
    pub(crate) web: Arc<Web>,
    library: Arc<dyn LikedSongs>,
    liker: Arc<dyn Liker>,
    presses: Presses,
    /// `None` once quitting: dropping the last sender stops Soloist.
    soloist: StdMutex<Option<SoloistTx>>,
    pub(crate) hints: mpsc::Sender<SessionHint>,
    saver: Saver,
    pub(crate) desktop: Arc<dyn Desktop>,
    windows: Arc<Windows>,
    liked_sync: LikedSync,
    quit: watch::Sender<bool>,
}

impl Engine {
    pub fn new(parts: Parts) -> Arc<Engine> {
        let player = watch::channel(parts.core.state.player.clone()).0;
        Arc::new(Engine {
            core: Mutex::new(parts.core),
            events: parts.events,
            player,
            pages: parts.pages,
            web: parts.web,
            library: parts.library,
            liker: parts.liker,
            presses: Presses::default(),
            soloist: StdMutex::new(Some(parts.soloist)),
            hints: parts.hints,
            saver: parts.saver,
            desktop: parts.desktop,
            windows: Arc::new(Windows::default()),
            liked_sync: LikedSync::default(),
            quit: watch::channel(false).0,
        })
    }

    /// The connected windows: whether one is open, and closing them.
    pub fn windows(&self) -> &Arc<Windows> {
        &self.windows
    }

    /// The state as of now and the events after it. Subscribed under the core lock, so
    /// no change falls between the two (one may come twice; every event is idempotent).
    pub async fn welcome(&self) -> (EngineState, broadcast::Receiver<Event>) {
        let core = self.core.lock().await;
        let events = self.events.subscribe();
        (core.state.clone(), events)
    }

    /// The player state as broadcast, from now on.
    pub fn player_state(&self) -> watch::Receiver<PlayerState> {
        self.player.subscribe()
    }

    /// Sends `event` to every connected client; a `Player` also goes to `player_state`.
    pub fn broadcast(&self, event: Event) {
        if let Event::Player(player) = &event {
            self.player.send_if_modified(|now| {
                let changed = now != player;
                if changed {
                    now.clone_from(player);
                }
                changed
            });
        }
        // No receivers just means no client is connected.
        let _ = self.events.send(event);
    }

    /// Runs `effects`, which `core` (still locked) just returned. Nothing here waits.
    pub(crate) fn execute(self: &Arc<Self>, core: &Core, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Broadcast(event) => self.broadcast(event),
                Effect::SetSoloistVolume(volume) => self.soloist_later(SoloistCmd::Volume(volume)),
                Effect::SaveSettings => self.saver.save(core.state.settings.clone()),
                Effect::CheckLiked(uri) => self.check_liked(uri),
                Effect::NotifyIfClosed(notice) => self.notify_if_closed(&notice.text, ""),
            }
        }
    }

    /// A desktop notification, unless a window is open to show it.
    pub(crate) fn notify_if_closed(&self, summary: &str, body: &str) {
        if !self.windows.window_connected() {
            self.desktop.notify(summary, body);
        }
    }

    /// Sends `cmd` to Soloist and waits for its outcome; `Err` is the text to show.
    pub async fn soloist(&self, cmd: SoloistCmd) -> Result<(), String> {
        let sender = self
            .soloist
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(sender) = sender else {
            return Err(QUITTING.into());
        };
        let (reply, answer) = oneshot::channel();
        sender
            .send((cmd, reply))
            .await
            .map_err(|_| QUITTING.to_string())?;
        // Not held while waiting, so a quit isn't held up by this command.
        drop(sender);
        answer.await.unwrap_or_else(|_| Err(PLAYER_STOPPED.into()))
    }

    /// `soloist(cmd)` on its own task, for effects: nothing that drains Soloist's events
    /// may wait for Soloist.
    fn soloist_later(self: &Arc<Self>, cmd: SoloistCmd) {
        let engine = self.clone();
        tokio::spawn(async move {
            let what = format!("{cmd:?}");
            if let Err(e) = engine.soloist(cmd).await {
                warn!("Soloist command {what}: {e}");
            }
        });
    }

    /// Closes the channel to Soloist: once the commands under way are answered, the
    /// loop stops Soloist and ends.
    pub fn close_soloist(&self) {
        self.soloist
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    /// Writes settings still waiting for their debounce.
    pub async fn flush_settings(&self) {
        self.saver.flush().await;
    }

    /// `Request::Quit`: the engine's main loop stops it.
    pub fn request_quit(&self) {
        self.quit.send_replace(true);
    }

    /// Returns once a quit was requested.
    pub async fn quit_requested(&self) {
        let mut quit = self.quit.subscribe();
        // The sender lives in `self`, so this can't fail while it is borrowed.
        let _ = quit.wait_for(|quit| *quit).await;
    }

    /// Tells the session supervisor about a failed request, if it says something about
    /// the session.
    pub(crate) fn hint(&self, e: &SpotError) {
        send_hint(&self.hints, e);
    }

    /// A heart press: `uri`, or the current track; made `want` (the window's heart), or
    /// toggled when `None` (the key, the tray, the CLI). `announce` shows the outcome as
    /// a desktop notification: for the CLI, the global key and the tray, not the window.
    /// After a failure the target's state is read back, since the write may have landed.
    pub async fn heart(
        self: &Arc<Self>,
        uri: Option<String>,
        want: Option<bool>,
        announce: bool,
    ) -> Result<Reply, String> {
        let target = match &uri {
            Some(uri) => Some(uri.clone()),
            None => self
                .core
                .lock()
                .await
                .state
                .player
                .track
                .as_ref()
                .map(|t| t.uri.clone()),
        };
        let result = {
            let _press = self.liked_sync.press();
            match (uri, want) {
                (Some(uri), Some(liked)) => {
                    self.presses.set(&self.core, &*self.liker, uri, liked).await
                }
                (uri, _) => self.presses.toggle(&self.core, &*self.liker, uri).await,
            }
        };
        if announce {
            self.desktop.notify(heart::announcement(&result), "");
        }
        match result {
            Ok(outcome) => {
                let core = self.core.lock().await;
                self.execute(&core, outcome.effects);
                Ok(Reply::Liked {
                    uri: outcome.uri,
                    liked: outcome.liked,
                })
            }
            Err(message) => {
                if let Some(uri) = target {
                    self.check_liked(uri);
                }
                Err(message)
            }
        }
    }

    /// `Effect::CheckLiked`: asks Spotify whether `uri` is liked, on its own task.
    fn check_liked(self: &Arc<Self>, uri: String) {
        let engine = self.clone();
        tokio::spawn(async move {
            let started = engine.liked_sync.read_starts();
            match engine
                .web
                .in_library(Lane::Background, std::slice::from_ref(&uri))
                .await
            {
                Ok(answers) => {
                    let Some(&liked) = answers.first() else {
                        return;
                    };
                    let mut core = engine.core.lock().await;
                    if !engine.liked_sync.still_current(started) {
                        debug!("liked check of {uri}: a heart press overlapped it; dropped");
                        return;
                    }
                    let effects = core.on_liked(&uri, liked);
                    engine.execute(&core, effects);
                }
                Err(e) => {
                    engine.hint(&e);
                    warn!("couldn't check whether {uri} is liked: {e}");
                }
            }
        });
    }

    /// Folds Soloist's events into the state until the Soloist loop ends. One batch of
    /// events broadcasts the player once, as it ends up. `Exited`, which the loop sends
    /// after the last of a Soloist's events once it has stopped, leaves the player
    /// paused where it had got to (`Core::on_player_down`).
    pub async fn on_playback(self: Arc<Self>, mut events: mpsc::Receiver<PlaybackEvent>) {
        let mut batch = Vec::new();
        while events.recv_many(&mut batch, 64).await > 0 {
            let mut core = self.core.lock().await;
            let mut effects = Vec::new();
            for event in batch.drain(..) {
                effects.extend(match event {
                    PlaybackEvent::Exited { .. } => core.on_player_down(now_ms()),
                    event => core.on_playback(&event),
                });
            }
            self.execute(&core, coalesce(effects));
        }
        debug!("Soloist's events ended");
    }

    /// Folds the session supervisor's updates into the state; `ready` follows whether
    /// the session is ready.
    pub async fn on_session(
        self: Arc<Self>,
        mut updates: mpsc::Receiver<SessionUpdate>,
        ready: watch::Sender<bool>,
    ) {
        while let Some(update) = updates.recv().await {
            let mut core = self.core.lock().await;
            if let Some(event) = session_update(&mut core.state, update) {
                self.broadcast(event);
            }
            let now_ready = core.state.session == SessionHealth::Ready;
            ready.send_if_modified(|was| std::mem::replace(was, now_ready) != now_ready);
        }
        debug!("the session supervisor stopped");
    }

    /// Follows Soloist's health until the Soloist loop ends.
    pub async fn on_soloist_health(self: Arc<Self>, mut health: watch::Receiver<SoloistHealth>) {
        loop {
            let now = health.borrow_and_update().clone();
            {
                let mut core = self.core.lock().await;
                if core.state.soloist != now {
                    info!("Soloist: {}", health_name(&now));
                    core.state.soloist = now.clone();
                    self.broadcast(Event::Soloist(now.clone()));
                    if let Some((summary, body)) = health_notification(&now) {
                        self.notify_if_closed(&summary, &body);
                    }
                }
            }
            if health.changed().await.is_err() {
                return;
            }
        }
    }

    /// Soloist's Info notices (the first-run lossless switch) to every client.
    pub async fn on_notices(self: Arc<Self>, mut notices: mpsc::Receiver<String>) {
        while let Some(text) = notices.recv().await {
            let notice = self.core.lock().await.notice(Level::Info, text);
            self.broadcast(Event::Notice(notice));
        }
    }

    /// An error notice for every client, and a desktop notification while no window is
    /// open to show it.
    pub async fn error_notice(self: &Arc<Self>, text: String) {
        let mut core = self.core.lock().await;
        let notice = core.notice(Level::Error, text);
        let effects = vec![
            Effect::Broadcast(Event::Notice(notice.clone())),
            Effect::NotifyIfClosed(notice),
        ];
        self.execute(&core, effects);
    }

    /// Reads Liked Songs into the liked set each time the session becomes ready, and
    /// every [`LIKED_EVERY`] while it stays so; until the first read lands, again after
    /// each of [`FIRST_READ_RETRIES`].
    pub async fn refresh_liked(self: Arc<Self>, mut ready: watch::Receiver<bool>) {
        let mut misses = 0;
        loop {
            let alive = ready.wait_for(|ready| *ready).await.is_ok();
            if !alive {
                return;
            }
            let wait = if self.refresh_liked_once().await {
                LIKED_EVERY
            } else {
                let wait = FIRST_READ_RETRIES.get(misses).copied();
                misses += 1;
                wait.unwrap_or(LIKED_EVERY)
            };
            tokio::select! {
                () = sleep(wait) => {}
                gone = async { ready.wait_for(|ready| !*ready).await.is_err() } => {
                    if gone {
                        return;
                    }
                }
            }
        }
    }

    /// Reads Liked Songs into the liked set, once: whether the set has been read whole
    /// by now (this time or before).
    async fn refresh_liked_once(self: &Arc<Self>) -> bool {
        let started = self.liked_sync.read_starts();
        match self.library.liked_uris(Lane::Background).await {
            Ok(uris) => {
                let count = uris.len();
                let mut core = self.core.lock().await;
                if !self.liked_sync.still_current(started) {
                    info!("Liked Songs refresh dropped: a heart press overlapped it");
                    return core.liked_filled();
                }
                let effects = if core.liked_filled() {
                    core.refresh_liked_set(uris)
                } else {
                    core.set_liked_set(uris)
                };
                debug!("Liked Songs: {count} tracks");
                self.execute(&core, effects);
                true
            }
            Err(e) => {
                self.hint(&e);
                warn!("couldn't read Liked Songs: {e}");
                self.core.lock().await.liked_filled()
            }
        }
    }

    /// Refreshes the pathfinder hashes when due: first after [`HASHES_FIRST_AFTER`],
    /// then every [`HASHES_EVERY`], on a fixed timer (`Ok` doesn't mean it scraped).
    pub async fn refresh_hashes(self: Arc<Self>) {
        let start = tokio::time::Instant::now() + HASHES_FIRST_AFTER;
        let mut every = interval_at(start, HASHES_EVERY);
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            if let Err(e) = self.web.refresh_hashes_if_due(now_ms()).await {
                warn!("refreshing the pathfinder hashes failed: {e}");
            }
        }
    }
}

/// Unix milliseconds now.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Keeps only the last `Player` broadcast among `effects`: one batch of Soloist's events
/// shows the player as it ends up. Everything else stays, in order.
pub fn coalesce(effects: Vec<Effect>) -> Vec<Effect> {
    let is_player = |e: &Effect| matches!(e, Effect::Broadcast(Event::Player(_)));
    let Some(last) = effects.iter().rposition(is_player) else {
        return effects;
    };
    effects
        .into_iter()
        .enumerate()
        .filter(|(i, e)| *i == last || !is_player(e))
        .map(|(_, e)| e)
        .collect()
}

/// Folds a session update into `state`; the event to broadcast, if any.
pub fn session_update(state: &mut EngineState, update: SessionUpdate) -> Option<Event> {
    match update {
        SessionUpdate::Health(health) => {
            if state.session == health {
                return None;
            }
            info!("session: {health:?}");
            state.session = health.clone();
            Some(Event::Session(health))
        }
        // A prompt or failure repeats when the owner tries again: always news.
        SessionUpdate::Login(step) => {
            state.login = step.clone();
            Some(Event::Login(step))
        }
        // Also told to the clients that connected before the login (the window that
        // started the engine, one open across an engine restart).
        SessionUpdate::Username(name) => {
            state.username = Some(name.clone());
            Some(Event::Username(name))
        }
    }
}

/// The desktop notification for a Soloist health that needs the owner while no window
/// is open: `(summary, body)`, in the window's words.
pub fn health_notification(health: &SoloistHealth) -> Option<(String, String)> {
    let install = |path: &str| {
        format!(
            "Run {UPDATE_COMMAND}, or download it from {DOWNLOADS_URL}\nand put the soloist binary at {path}."
        )
    };
    match health {
        SoloistHealth::Failed { message, .. } => Some((message.clone(), String::new())),
        SoloistHealth::Expired { path } => {
            Some(("This Soloist build has expired.".into(), install(path)))
        }
        SoloistHealth::BinaryMissing { path } => {
            Some(("Spotify Soloist isn't installed.".into(), install(path)))
        }
        SoloistHealth::Starting
        | SoloistHealth::Ready
        | SoloistHealth::NotPaired
        | SoloistHealth::Reconnecting { .. } => None,
    }
}

/// A health's name for the log, without its log tail.
fn health_name(health: &SoloistHealth) -> String {
    match health {
        SoloistHealth::Reconnecting {
            attempt,
            retry_in_ms,
        } => format!("restarting (attempt {attempt}, in {retry_in_ms} ms)"),
        SoloistHealth::Failed { message, .. } => format!("failed: {message}"),
        other => format!("{other:?}"),
    }
}

/// Heart presses and reads of the liked state, so a read that a press overlapped is
/// dropped instead of undoing the press. A read records `read_starts()` before it asks
/// Spotify and applies its answer only if `still_current` holds, under the core lock.
#[derive(Default)]
pub struct LikedSync {
    /// Presses under way.
    presses: AtomicU64,
    /// Bumped as each press ends.
    ended: AtomicU64,
}

impl LikedSync {
    /// Marks a press under way until the guard drops.
    pub fn press(&self) -> Press<'_> {
        self.presses.fetch_add(1, Ordering::SeqCst);
        Press(self)
    }

    /// Where a read starts.
    pub fn read_starts(&self) -> u64 {
        self.ended.load(Ordering::SeqCst)
    }

    /// Whether a read that started at `started` may be applied: no press is under way,
    /// and none ended since it started.
    pub fn still_current(&self, started: u64) -> bool {
        // `presses` first: a press bumps `ended` before it stops counting as under way,
        // so one that ends between the two loads is caught by the second.
        self.presses.load(Ordering::SeqCst) == 0 && self.ended.load(Ordering::SeqCst) == started
    }
}

/// A heart press under way.
pub struct Press<'a>(&'a LikedSync);

impl Drop for Press<'_> {
    fn drop(&mut self) {
        self.0.ended.fetch_add(1, Ordering::SeqCst);
        self.0.presses.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! An engine on fakes: no Soloist, no session, no network, no desktop.

    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use engine_ipc::{
        Settings,
        proto::{Page, PageRequest},
    };
    use std::collections::VecDeque;

    use spot_core::web::{
        Tokens,
        lanes::{BACKGROUND_GAP, Lanes},
        registry::HashRegistry,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::pages::PageSource;

    /// Every Spotify call fails at once with `NotLoggedIn`.
    pub struct NoSpotify;

    #[async_trait]
    impl Tokens for NoSpotify {
        async fn bearer(&self) -> spot_core::Result<String> {
            Err(SpotError::NotLoggedIn)
        }
        async fn client_token(&self) -> spot_core::Result<String> {
            Err(SpotError::NotLoggedIn)
        }
    }

    #[async_trait]
    impl PageSource for NoSpotify {
        async fn fetch(&self, _: Lane, _: &PageRequest) -> spot_core::Result<Page> {
            Err(SpotError::NotLoggedIn)
        }
    }

    /// Heart writes answer `Ok`, or `Err(fail())` when `fail` is set; each is recorded.
    /// A read of a track's state says whether it is in `on_account`.
    #[derive(Default)]
    pub struct FakeLiker {
        pub calls: StdMutex<Vec<(String, bool)>>,
        pub fail: Option<fn() -> SpotError>,
        pub on_account: Vec<String>,
    }

    #[async_trait]
    impl Liker for FakeLiker {
        async fn is_liked(&self, uri: &str) -> spot_core::Result<bool> {
            Ok(self.on_account.iter().any(|liked| liked == uri))
        }

        async fn set_liked(&self, uri: &str, liked: bool) -> spot_core::Result<()> {
            self.calls.lock().unwrap().push((uri.into(), liked));
            match self.fail {
                Some(fail) => Err(fail()),
                None => Ok(()),
            }
        }
    }

    /// Liked Songs as scripted: each read takes the next answer, `NotLoggedIn` once they
    /// run out; when each read came is recorded.
    #[derive(Default)]
    pub struct FakeLikedSongs {
        pub answers: StdMutex<VecDeque<spot_core::Result<Vec<String>>>>,
        pub reads: StdMutex<Vec<tokio::time::Instant>>,
    }

    impl FakeLikedSongs {
        /// Reads answering `answers`, in turn.
        pub fn answering(answers: Vec<spot_core::Result<Vec<String>>>) -> Self {
            FakeLikedSongs {
                answers: StdMutex::new(answers.into()),
                reads: StdMutex::default(),
            }
        }

        pub fn reads(&self) -> Vec<tokio::time::Instant> {
            self.reads.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LikedSongs for FakeLikedSongs {
        async fn liked_uris(&self, _: Lane) -> spot_core::Result<Vec<String>> {
            self.reads.lock().unwrap().push(tokio::time::Instant::now());
            let next = self.answers.lock().unwrap().pop_front();
            next.unwrap_or(Err(SpotError::NotLoggedIn))
        }
    }

    /// Records what the engine asks of the desktop.
    #[derive(Default)]
    pub struct FakeDesktop {
        pub notified: StdMutex<Vec<String>>,
        pub autostart: StdMutex<Vec<bool>>,
        pub forgot: AtomicU64,
    }

    #[async_trait]
    impl Desktop for FakeDesktop {
        fn notify(&self, summary: &str, _body: &str) {
            self.notified.lock().unwrap().push(summary.into());
        }
        async fn set_autostart(&self, on: bool) -> Result<(), String> {
            self.autostart.lock().unwrap().push(on);
            Ok(())
        }
        async fn forget_soloist_key(&self) -> Result<(), String> {
            self.forgot.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl FakeDesktop {
        pub fn notified(&self) -> Vec<String> {
            self.notified.lock().unwrap().clone()
        }
    }

    pub struct TestEngine {
        pub engine: Arc<Engine>,
        /// What the engine sends Soloist; answer each on its `CmdReply`.
        pub soloist: mpsc::Receiver<(SoloistCmd, CmdReply)>,
        pub hints: mpsc::Receiver<SessionHint>,
        pub desktop: Arc<FakeDesktop>,
        pub liker: Arc<FakeLiker>,
        /// What `refresh_liked` reads; the tests that run it script it.
        pub liked_songs: Arc<FakeLikedSongs>,
        pub dir: TempDir,
    }

    pub fn settings() -> Settings {
        Settings {
            volume: 40,
            output_device: None,
            autostart: false,
        }
    }

    pub async fn test_engine(core: Core) -> TestEngine {
        test_engine_with(core, FakeLiker::default()).await
    }

    pub async fn test_engine_with(core: Core, liker: FakeLiker) -> TestEngine {
        test_engine_full(core, liker, FakeLikedSongs::default()).await
    }

    pub async fn test_engine_full(
        core: Core,
        liker: FakeLiker,
        liked_songs: FakeLikedSongs,
    ) -> TestEngine {
        let dir = tempfile::tempdir().unwrap();
        let (events, _) = broadcast::channel(64);
        let lanes = Arc::new(Lanes::new(BACKGROUND_GAP));
        let registry = HashRegistry::load(dir.path().join("hashes.json")).await;
        let web = Arc::new(Web::new(
            Arc::new(NoSpotify),
            lanes.clone(),
            Arc::new(registry),
        ));
        let liked_songs = Arc::new(liked_songs);
        let pages = PageService::new(
            Arc::new(NoSpotify),
            dir.path().join("pages"),
            events.clone(),
        );
        let (soloist_tx, soloist) = mpsc::channel(8);
        let (hints_tx, hints) = mpsc::channel(8);
        let desktop = Arc::new(FakeDesktop::default());
        let liker = Arc::new(liker);
        let engine = Engine::new(Parts {
            core,
            events,
            pages,
            web,
            library: liked_songs.clone(),
            liker: liker.clone(),
            soloist: soloist_tx,
            hints: hints_tx,
            saver: Saver::new(dir.path().join("settings.json")),
            desktop: desktop.clone(),
        });
        TestEngine {
            engine,
            soloist,
            hints,
            desktop,
            liker,
            liked_songs,
            dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use engine_ipc::proto::{LoginStep, Notice, PlayerState};
    use playback::event::{NowPlaying, PlayStatus};

    use super::{testing::*, *};

    fn player(volume: u8) -> PlayerState {
        let mut player = Core::new(settings()).state.player;
        player.volume = volume;
        player
    }

    #[test]
    fn coalesce_keeps_the_last_player() {
        let queue = Effect::Broadcast(Event::Queue(vec![]));
        let effects = vec![
            Effect::Broadcast(Event::Player(player(1))),
            Effect::SetSoloistVolume(40),
            queue.clone(),
            Effect::Broadcast(Event::Player(player(2))),
            Effect::CheckLiked("spotify:track:t".into()),
            Effect::Broadcast(Event::Player(player(3))),
        ];
        assert_eq!(
            coalesce(effects),
            [
                Effect::SetSoloistVolume(40),
                queue,
                Effect::CheckLiked("spotify:track:t".into()),
                Effect::Broadcast(Event::Player(player(3))),
            ]
        );
        assert_eq!(coalesce(vec![]), []);
    }

    #[test]
    fn session_updates_fill_the_state() {
        let mut state = Core::new(settings()).state;
        assert_eq!(
            session_update(&mut state, SessionUpdate::Login(LoginStep::Needed)),
            Some(Event::Login(LoginStep::Needed))
        );
        assert_eq!(state.login, LoginStep::Needed);
        // Final review I1: a client that connected before the login hears the name.
        assert_eq!(
            session_update(&mut state, SessionUpdate::Username("owner".into())),
            Some(Event::Username("owner".into()))
        );
        assert_eq!(state.username.as_deref(), Some("owner"));
        assert_eq!(
            session_update(&mut state, SessionUpdate::Health(SessionHealth::Ready)),
            Some(Event::Session(SessionHealth::Ready))
        );
        assert_eq!(state.session, SessionHealth::Ready);
        // The same health again is no news.
        assert_eq!(
            session_update(&mut state, SessionUpdate::Health(SessionHealth::Ready)),
            None
        );
    }

    #[test]
    fn health_notifications_in_the_windows_words() {
        for quiet in [
            SoloistHealth::Starting,
            SoloistHealth::Ready,
            SoloistHealth::NotPaired,
            SoloistHealth::Reconnecting {
                attempt: 1,
                retry_in_ms: 1000,
            },
        ] {
            assert_eq!(health_notification(&quiet), None, "{quiet:?}");
        }
        let failed = SoloistHealth::Failed {
            message: "Spotify Soloist keeps stopping (exit code 1).".into(),
            log_tail: "the tail".into(),
        };
        assert_eq!(
            health_notification(&failed),
            Some((
                "Spotify Soloist keeps stopping (exit code 1).".into(),
                String::new()
            ))
        );
        let path = "/home/u/.local/share/loudauto/soloist/soloist".to_string();
        let body = format!(
            "Run {UPDATE_COMMAND}, or download it from {DOWNLOADS_URL}\nand put the soloist binary at {path}."
        );
        assert_eq!(
            health_notification(&SoloistHealth::Expired { path: path.clone() }),
            Some(("This Soloist build has expired.".into(), body.clone()))
        );
        assert_eq!(
            health_notification(&SoloistHealth::BinaryMissing { path }),
            Some(("Spotify Soloist isn't installed.".into(), body))
        );
    }

    #[test]
    fn a_press_overlapping_a_read_drops_it() {
        let sync = LikedSync::default();
        // Nothing happened: the read applies.
        let read = sync.read_starts();
        assert!(sync.still_current(read));

        // A press under way when the read lands: dropped.
        let read = sync.read_starts();
        let press = sync.press();
        assert!(!sync.still_current(read));
        // A press that ended while the read was out: dropped.
        drop(press);
        assert!(!sync.still_current(read));

        // A press under way when the read started, still under way: dropped.
        let press = sync.press();
        let read = sync.read_starts();
        assert!(!sync.still_current(read));
        drop(press);
        assert!(!sync.still_current(read));

        // After it, a fresh read applies again.
        let read = sync.read_starts();
        assert!(sync.still_current(read));
    }

    fn nerve() -> NowPlaying {
        NowPlaying {
            uri: "spotify:track:1XD4K4CGAKTIBmFpvuaFru".into(),
            title: "nerve".into(),
            artists: vec!["2hollis".into()],
            artist_uris: vec![],
            album: None,
            album_uri: None,
            duration_ms: Some(150_000),
            cover_url: None,
        }
    }

    /// Final review I2: Soloist stopping (`Active(false)`, then `Exited`, after all it
    /// said) shows the player paused where it had got to, for MPRIS and the tray too.
    #[tokio::test]
    async fn a_stopped_soloist_leaves_the_player_paused() {
        let t = test_engine(Core::new(settings())).await;
        let mpris = t.engine.player_state();
        let (to_core, rx) = mpsc::channel(8);
        tokio::spawn(t.engine.clone().on_playback(rx));
        for event in [
            PlaybackEvent::Track(nerve()),
            PlaybackEvent::Status(PlayStatus::Playing),
            PlaybackEvent::Position(playback::event::Position {
                position_ms: 1_000,
                timestamp_ms: now_ms(),
                speed: 1.0,
            }),
            PlaybackEvent::Active(false),
            PlaybackEvent::Exited {
                code: None,
                tail: String::new(),
            },
        ] {
            to_core.send(event).await.unwrap();
        }
        drop(to_core);
        let mut mpris = mpris;
        tokio::time::timeout(
            Duration::from_secs(5),
            mpris.wait_for(|p| p.status == PlayStatus::Paused),
        )
        .await
        .expect("still playing")
        .unwrap();
        let player = t.engine.core.lock().await.state.player.clone();
        assert_eq!(player.status, PlayStatus::Paused);
        assert!(player.position.unwrap().position_ms >= 1_000);
    }

    #[tokio::test]
    async fn effects_notify_only_while_no_window_is_open() {
        let t = test_engine(Core::new(settings())).await;
        let notice = Notice {
            id: 1,
            level: Level::Error,
            text: "Soloist said no.".into(),
        };
        {
            let core = t.engine.core.lock().await;
            t.engine
                .execute(&core, vec![Effect::NotifyIfClosed(notice.clone())]);
        }
        assert_eq!(t.desktop.notified(), ["Soloist said no."]);

        let _window = t.engine.windows().join();
        {
            let core = t.engine.core.lock().await;
            t.engine
                .execute(&core, vec![Effect::NotifyIfClosed(notice)]);
        }
        assert_eq!(t.desktop.notified(), ["Soloist said no."]);
    }

    #[tokio::test]
    async fn soloist_health_is_broadcast_and_notified_while_closed() {
        let t = test_engine(Core::new(settings())).await;
        let (_, mut events) = t.engine.welcome().await;
        let (health_tx, health_rx) = watch::channel(SoloistHealth::Starting);
        let watcher = tokio::spawn(t.engine.clone().on_soloist_health(health_rx));
        let missing = SoloistHealth::BinaryMissing {
            path: "/x/soloist".into(),
        };
        health_tx.send_replace(missing.clone());
        assert_eq!(
            events.recv().await.unwrap(),
            Event::Soloist(missing.clone())
        );
        drop(health_tx);
        watcher.await.unwrap();
        assert_eq!(t.desktop.notified(), ["Spotify Soloist isn't installed."]);
        assert_eq!(t.engine.core.lock().await.state.soloist, missing);
    }

    #[tokio::test]
    async fn heart_announces_for_the_cli_never_the_window() {
        let mut core = Core::new(settings());
        core.on_playback(&PlaybackEvent::Track(nerve()));
        core.on_playback(&PlaybackEvent::Status(PlayStatus::Playing));
        let t = test_engine(core).await;

        let liked = t.engine.heart(None, None, false).await;
        assert_eq!(
            liked,
            Ok(Reply::Liked {
                uri: nerve().uri,
                liked: true
            })
        );
        assert!(
            t.desktop.notified().is_empty(),
            "the window's press announced"
        );
        assert_eq!(
            t.engine.core.lock().await.is_liked(&nerve().uri),
            Some(true)
        );

        let unliked = t.engine.heart(None, None, true).await;
        assert_eq!(
            unliked,
            Ok(Reply::Liked {
                uri: nerve().uri,
                liked: false
            })
        );
        assert_eq!(
            t.desktop.notified(),
            ["Removed from Liked Songs: nerve — 2hollis"]
        );
        assert_eq!(
            *t.liker.calls.lock().unwrap(),
            [(nerve().uri, true), (nerve().uri, false)]
        );
    }

    /// Final review C1: a first read of Liked Songs that fails is tried again after 5,
    /// 15 and 60 s, not 10 minutes; once it lands clients are told (`LikedSetReady`), and
    /// the reads go back to every 10 minutes.
    #[tokio::test(start_paused = true)]
    async fn a_failed_first_liked_read_is_tried_again_soon() {
        let net = || Err(SpotError::Net("no route".into()));
        let songs = FakeLikedSongs::answering(vec![net(), net(), net(), Ok(vec![nerve().uri])]);
        let t = test_engine_full(Core::new(settings()), FakeLiker::default(), songs).await;
        let (_, mut events) = t.engine.welcome().await;
        let (_ready_tx, ready) = watch::channel(true);
        tokio::spawn(t.engine.clone().refresh_liked(ready));
        loop {
            if events.recv().await.unwrap() == Event::LikedSetReady {
                break;
            }
        }
        assert_eq!(
            t.engine.core.lock().await.is_liked(&nerve().uri),
            Some(true)
        );
        while t.liked_songs.reads().len() < 5 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let reads = t.liked_songs.reads();
        let gaps: Vec<Duration> = reads.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(
            gaps,
            [
                Duration::from_secs(5),
                Duration::from_secs(15),
                Duration::from_secs(60),
                LIKED_EVERY
            ]
        );
    }

    #[tokio::test]
    async fn failed_heart_is_announced_and_changes_nothing() {
        let mut core = Core::new(settings());
        core.on_playback(&PlaybackEvent::Track(nerve()));
        let liker = FakeLiker {
            fail: Some(|| SpotError::Http {
                status: 503,
                what: "/pathfinder/v2/query".into(),
            }),
            ..FakeLiker::default()
        };
        let t = test_engine_with(core, liker).await;
        let liked = t.engine.heart(None, None, true).await;
        assert_eq!(liked, Err("Couldn't like: Spotify answered 503".into()));
        assert_eq!(
            t.desktop.notified(),
            ["Couldn't like: Spotify answered 503"]
        );
        assert_eq!(t.engine.core.lock().await.is_liked(&nerve().uri), None);
    }
}
