//! Soloist supervision: launches Spotify Soloist, reports its health, restarts it after
//! it stops on its own, and turns the engine's commands into Soloist commands (spec:
//! "Soloist is supervised", "First run, login, settings", "Starting a list at a chosen
//! track").
//!
//! - Launch → `Starting`; the first logged-in `Auth` → `Ready`; still logged out
//!   `NOT_PAIRED_AFTER` after the start (and Soloist said so) → `NotPaired`.
//! - Soloist stops on its own → `Active(false)` and `Exited` to core (which shows the
//!   player paused where it had got to) and the `RestartPolicy`'s decision:
//!   `Reconnecting{attempt, retry_in_ms}` then a relaunch, `Expired{path}`, or
//!   `Failed{message, log_tail}`. Soloist comes back paused and is never told to play.
//!   Stops the loop makes itself (the prefs restart, `Restart`, the engine quitting) are
//!   not failures and never reach the policy.
//! - `Failed`, `Expired` and `BinaryMissing` stay until `SoloistCmd::Restart`, which
//!   relaunches with a fresh policy.
//! - First session: Soloist creates its user dir at its first login, so its quality prefs
//!   can't be set before that start. When a launch found no user dir, the first login
//!   rewrites them and restarts Soloist once, with the notice `LOSSLESS_NOTICE`.
//! - Commands are answered on their `CmdReply`; while not `Ready`, with `RESTARTING`.
//! - Closing the command channel stops Soloist and ends the loop (engine quit).

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use engine_ipc::SoloistHealth;
use log::{debug, info, warn};
use playback::{
    SoloistError,
    backend::{LoginGate, Playback, SoloistBackend},
    client::CONNECTION_CLOSED,
    command::Repeat,
    config::{KeyCache, SoloistConfig},
    event::PlaybackEvent,
    prefs::{PrefsOutcome, ensure_quality_prefs_async},
    restart::{Decision, RestartPolicy},
    supervisor::running_soloist,
};
use spot_core::web::{ConnectPlay, Web, random_hex};
use tokio::{
    sync::{broadcast::error::RecvError, mpsc, oneshot, watch},
    time::{Instant, sleep},
};

/// The reply to a command sent while Soloist isn't `Ready`.
pub const RESTARTING: &str = "The player is restarting.";
/// The Info notice after the first-session prefs fix.
pub const LOSSLESS_NOTICE: &str = "Switched the player to lossless; it restarted once.";

/// Soloist running this long without stopping is healthy again: the next restart waits
/// the first backoff (`RestartPolicy::on_healthy`).
const HEALTHY_AFTER: Duration = Duration::from_secs(120);
/// Soloist still logged out this long after its start was never paired.
const NOT_PAIRED_AFTER: Duration = Duration::from_secs(10);
/// How long a closed WebSocket waits for Soloist's `Exited`. It normally comes within a
/// second (the supervisor drains Soloist's output first); a Soloist still running past
/// this can't be controlled any more, so it is restarted.
const CLOSED_GRACE: Duration = Duration::from_secs(3);
/// The `Failed` message when Soloist's WebSocket closed and it kept running.
const LOST: &str = "Lost the connection to Spotify Soloist.";
/// The Connect `from` id's length in bytes (spec: a random, unregistered id).
const FROM_ID_BYTES: usize = 20;

/// Where a command's outcome goes: `Err` carries the text to show.
pub type CmdReply = oneshot::Sender<Result<(), String>>;

/// What the engine asks of Soloist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoloistCmd {
    /// `None` resumes (also what another device plays); a URI pulls playback here
    /// (`activate`, then `play`).
    Play(Option<String>),
    /// A context from a chosen track, by the Connect play command.
    PlayContext(ConnectPlay),
    Pause,
    Next,
    Prev,
    Seek(u64),
    Volume(u8),
    Shuffle(bool),
    Repeat(Repeat),
    Queue(String),
    Activate,
    /// Ask for auth state, playback state and the queue.
    Refresh,
    /// Relaunch Soloist with a fresh restart policy: "Try again" after `Failed`.
    Restart,
}

/// Sends the Connect play command (`Web::connect_play`); a trait so tests can stub it.
#[async_trait]
pub trait ConnectPlayer: Send + Sync {
    async fn connect_play(&self, from: &str, to: &str, p: &ConnectPlay) -> spot_core::Result<()>;
}

#[async_trait]
impl ConnectPlayer for Web {
    async fn connect_play(&self, from: &str, to: &str, p: &ConnectPlay) -> spot_core::Result<()> {
        Web::connect_play(self, from, to, p).await
    }
}

/// Soloist's Connect device id: the file `.device_id` in its data dir, trimmed.
pub fn read_device_id(data_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join(".device_id")).ok()?;
    Some(text.trim().to_string()).filter(|id| !id.is_empty())
}

/// The loop's delays; only tests change them.
#[derive(Debug, Clone, Copy)]
struct Timings {
    not_paired_after: Duration,
    healthy_after: Duration,
    closed_grace: Duration,
    /// Each restart wait is divided by this. The health still shows the policy's wait.
    wait_divisor: u32,
}

impl Timings {
    const ENGINE: Timings = Timings {
        not_paired_after: NOT_PAIRED_AFTER,
        healthy_after: HEALTHY_AFTER,
        closed_grace: CLOSED_GRACE,
        wait_divisor: 1,
    };
}

/// Supervises Soloist for as long as the engine runs. `to_core` gets Soloist's events
/// (`ExpiresInDays` included; Soloist's own `Exited` is not passed on), and whenever
/// Soloist stops, after the last of its events, `Active(false)` then `Exited`; `health` its
/// state; `notices` Info notices to show. Each `wake` change asks Soloist for fresh
/// state. Returns, after stopping Soloist, once `cmds` or `to_core` closes.
#[allow(clippy::too_many_arguments)] // the engine wires each channel on its own
pub async fn run_soloist(
    cfg: SoloistConfig,
    keys: Arc<dyn KeyCache>,
    connect: Arc<dyn ConnectPlayer>,
    to_core: mpsc::Sender<PlaybackEvent>,
    health: watch::Sender<SoloistHealth>,
    cmds: mpsc::Receiver<(SoloistCmd, CmdReply)>,
    wake: watch::Receiver<u64>,
    notices: mpsc::Sender<String>,
) {
    Soloist::new(cfg, keys, connect, to_core, health, wake, notices)
        .run(cmds)
        .await;
    debug!("Soloist supervisor stopped");
}

type Commands = mpsc::Receiver<(SoloistCmd, CmdReply)>;

/// What the loop does next.
enum Next {
    Launch,
    /// Show `Reconnecting` (already set), wait this long, then launch.
    Wait(Duration),
    /// Keep the current health until `Restart`.
    Halt,
    Quit,
}

/// One way Soloist failed, for the policy and, if it gives up, the `Failed` screen.
struct Failure {
    code: Option<i32>,
    message: String,
    tail: String,
}

impl Failure {
    fn exit(code: Option<i32>, tail: String) -> Failure {
        let how = match code {
            Some(code) => format!("exit code {code}"),
            None => "killed by a signal".to_string(),
        };
        Failure {
            code,
            message: format!("Spotify Soloist keeps stopping ({how})."),
            tail,
        }
    }

    /// Soloist's WebSocket or event stream went away while it kept running.
    fn lost() -> Failure {
        Failure {
            code: None,
            message: LOST.to_string(),
            tail: String::new(),
        }
    }

    /// A start that failed without Soloist exiting by itself (no WebSocket in time,
    /// another Soloist on the data dir, …): a failure like any exit.
    fn other(e: &SoloistError) -> Failure {
        Failure {
            code: None,
            message: e.to_string(),
            tail: String::new(),
        }
    }
}

struct Soloist {
    cfg: SoloistConfig,
    keys: Arc<dyn KeyCache>,
    connect: Arc<dyn ConnectPlayer>,
    to_core: mpsc::Sender<PlaybackEvent>,
    health: watch::Sender<SoloistHealth>,
    wake: watch::Receiver<u64>,
    /// False once `wake` says no wake will come (no logind).
    wake_alive: bool,
    notices: mpsc::Sender<String>,
    timings: Timings,
    /// The Connect controller id, one per engine run.
    from_id: String,
    policy: RestartPolicy,
    /// Restarts since the last healthy run or fresh policy, for `Reconnecting`.
    attempt: u32,
    /// The first-session prefs restart happened; it happens at most once per run.
    lossless_done: bool,
}

impl Soloist {
    fn new(
        cfg: SoloistConfig,
        keys: Arc<dyn KeyCache>,
        connect: Arc<dyn ConnectPlayer>,
        to_core: mpsc::Sender<PlaybackEvent>,
        health: watch::Sender<SoloistHealth>,
        wake: watch::Receiver<u64>,
        notices: mpsc::Sender<String>,
    ) -> Self {
        Soloist {
            cfg,
            keys,
            connect,
            to_core,
            health,
            wake,
            wake_alive: true,
            notices,
            timings: Timings::ENGINE,
            from_id: random_hex(FROM_ID_BYTES),
            policy: RestartPolicy::new(),
            attempt: 0,
            lossless_done: false,
        }
    }

    async fn run(mut self, mut cmds: Commands) {
        let mut next = Next::Launch;
        loop {
            next = match next {
                Next::Launch => self.launch_and_supervise(&mut cmds).await,
                Next::Wait(wait) => self.wait(wait, &mut cmds).await,
                Next::Halt => self.halt(&mut cmds).await,
                Next::Quit => return,
            };
        }
    }

    async fn launch_and_supervise(&mut self, cmds: &mut Commands) -> Next {
        self.set_health(SoloistHealth::Starting);
        let first_session = !self.lossless_done && self.first_session().await;
        match self.launch(cmds).await {
            None => Next::Quit,
            Some(Ok(backend)) => self.supervise(backend, first_session, cmds).await,
            Some(Err(e)) => self.launch_failed(e),
        }
    }

    /// Whether this start is Soloist's first session: no user dir, so its prefs can't be
    /// set before it starts (the launch sets them whenever they can be). A data dir that
    /// another Soloist holds is left alone; that launch fails with `AlreadyRunning`.
    async fn first_session(&self) -> bool {
        let data_dir = self.cfg.data_dir.clone();
        let other = tokio::task::spawn_blocking(move || running_soloist(&data_dir))
            .await
            .ok()
            .flatten();
        if other.is_some() {
            return false;
        }
        match ensure_quality_prefs_async(&self.cfg.data_dir).await {
            Ok(outcome) => outcome == PrefsOutcome::NoUser,
            Err(e) => {
                warn!("could not check Soloist's quality prefs: {e}");
                false
            }
        }
    }

    /// Launch Soloist. Commands that come meanwhile are refused (`Restart` only resets
    /// the policy: this is a fresh start already). `None` if `cmds` closed.
    async fn launch(&mut self, cmds: &mut Commands) -> Option<playback::Result<SoloistBackend>> {
        let keys = self.keys.clone();
        let launch = SoloistBackend::launch(self.cfg.clone(), &*keys);
        tokio::pin!(launch);
        loop {
            tokio::select! {
                launched = &mut launch => return Some(launched),
                cmd = cmds.recv() => {
                    if let Some(Next::Quit) = self.while_down(cmd) {
                        return None;
                    }
                }
            }
        }
    }

    fn launch_failed(&mut self, e: SoloistError) -> Next {
        warn!("Soloist did not start: {e}");
        match e {
            SoloistError::BinaryMissing(path) => {
                self.set_health(SoloistHealth::BinaryMissing {
                    path: path.display().to_string(),
                });
                Next::Halt
            }
            SoloistError::Expired => {
                self.set_health(self.expired());
                Next::Halt
            }
            // Trying again would look the key up again and again: wait for "Try again".
            SoloistError::Key(_) => {
                self.set_health(SoloistHealth::Failed {
                    message: e.to_string(),
                    log_tail: String::new(),
                });
                Next::Halt
            }
            SoloistError::Exited { code, tail } => self.failed(Failure::exit(code, tail)),
            e => self.failed(Failure::other(&e)),
        }
    }

    /// Soloist failed: ask the policy what now.
    fn failed(&mut self, failure: Failure) -> Next {
        match self.policy.on_exit(failure.code, std::time::Instant::now()) {
            Decision::RestartAfter(wait) => {
                self.attempt += 1;
                info!("restarting Soloist in {wait:?} (attempt {})", self.attempt);
                self.set_health(SoloistHealth::Reconnecting {
                    attempt: self.attempt,
                    retry_in_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                });
                Next::Wait(wait)
            }
            Decision::Expired => {
                self.set_health(self.expired());
                Next::Halt
            }
            Decision::GiveUp => {
                warn!(
                    "Soloist keeps failing; not restarting it: {}",
                    failure.message
                );
                self.set_health(SoloistHealth::Failed {
                    message: failure.message,
                    log_tail: failure.tail,
                });
                Next::Halt
            }
        }
    }

    /// Watch a running Soloist until it stops, or the loop stops it.
    async fn supervise(
        &mut self,
        backend: SoloistBackend,
        first_session: bool,
        cmds: &mut Commands,
    ) -> Next {
        // First, so the startup backlog (`ExpiresInDays`) comes to this receiver.
        let mut events = backend.subscribe();
        if let Err(e) = backend.refresh().await {
            warn!("could not ask Soloist for its state: {e}");
        }
        self.wake.borrow_and_update();
        let mut gate = LoginGate::default();
        let mut ready = false;
        let mut said_logged_out = false;
        let mut not_paired_due = false;
        let not_paired = sleep(self.timings.not_paired_after);
        let mut healthy_done = false;
        let healthy = sleep(self.timings.healthy_after);
        // Armed when the WebSocket closes; reset to the grace then.
        let mut lost_armed = false;
        let lost = sleep(Duration::ZERO);
        tokio::pin!(not_paired, healthy, lost);

        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(PlaybackEvent::Exited { code, tail }) => {
                        info!("Soloist exited ({code:?})");
                        self.stop(backend).await;
                        return self.failed(Failure::exit(code, tail));
                    }
                    Ok(PlaybackEvent::Error(message)) if message == CONNECTION_CLOSED => {
                        // Not an error to show: health says what happens next.
                        if !lost_armed {
                            debug!("Soloist's WebSocket closed; waiting for it to exit");
                            lost.as_mut().reset(Instant::now() + self.timings.closed_grace);
                            lost_armed = true;
                        }
                    }
                    Ok(event) => {
                        if gate.opens(&event) {
                            // `first_session` is false once the fix has run this engine run.
                            if first_session && self.rewrote_prefs().await {
                                self.lossless_done = true;
                                info!("switched Soloist to lossless; restarting it once");
                                self.stop(backend).await;
                                let _ = self.notices.send(LOSSLESS_NOTICE.to_string()).await;
                                return Next::Launch;
                            }
                            ready = true;
                            self.set_health(SoloistHealth::Ready);
                            if let Err(e) = backend.refresh().await {
                                warn!("could not ask Soloist for its state: {e}");
                            }
                        }
                        if event == (PlaybackEvent::Auth { logged_in: false }) {
                            said_logged_out = true;
                        }
                        if !ready && said_logged_out && not_paired_due {
                            self.set_health(SoloistHealth::NotPaired);
                        }
                        if gate.hides(&event) {
                            continue;
                        }
                        if self.to_core.send(event).await.is_err() {
                            self.stop(backend).await;
                            return Next::Quit;
                        }
                    }
                    Err(RecvError::Lagged(n)) => warn!("missed {n} Soloist events"),
                    // The backend holds a sender, so this can't happen while it lives.
                    Err(RecvError::Closed) => {
                        self.stop(backend).await;
                        return self.failed(Failure::lost());
                    }
                },
                cmd = cmds.recv() => match cmd {
                    None => {
                        self.stop(backend).await;
                        return Next::Quit;
                    }
                    Some((SoloistCmd::Restart, reply)) => {
                        info!("restarting Soloist on request");
                        self.stop(backend).await;
                        self.fresh_policy();
                        let _ = reply.send(Ok(()));
                        return Next::Launch;
                    }
                    Some((cmd, reply)) if ready => self.execute(&backend, cmd, reply).await,
                    Some((_, reply)) => {
                        let _ = reply.send(Err(RESTARTING.to_string()));
                    }
                },
                () = &mut not_paired, if !not_paired_due => {
                    not_paired_due = true;
                    if !ready && said_logged_out {
                        self.set_health(SoloistHealth::NotPaired);
                    }
                }
                () = &mut healthy, if !healthy_done => {
                    healthy_done = true;
                    debug!(
                        "Soloist ran {:?}: back to the first backoff",
                        self.timings.healthy_after
                    );
                    self.policy.on_healthy();
                    self.attempt = 0;
                }
                woke = self.wake.changed(), if self.wake_alive => match woke {
                    Ok(()) => {
                        info!("resumed from sleep: asking Soloist for its state");
                        if let Err(e) = backend.refresh().await {
                            warn!("could not ask Soloist for its state: {e}");
                        }
                    }
                    Err(_) => self.wake_alive = false,
                },
                () = &mut lost, if lost_armed => {
                    // Soloist's own failure, not a stop the loop chose: it counts.
                    warn!("Soloist's WebSocket closed but Soloist kept running; restarting it");
                    self.stop(backend).await;
                    return self.failed(Failure::lost());
                }
            }
        }
    }

    /// The first-session fix, at the first login: whether it rewrote the prefs. Soloist
    /// is running, but the relaunch that follows sets them again before it starts, in
    /// case Soloist saves its own over them when it stops.
    async fn rewrote_prefs(&self) -> bool {
        match ensure_quality_prefs_async(&self.cfg.data_dir).await {
            Ok(PrefsOutcome::Updated) => true,
            Ok(PrefsOutcome::AlreadySet) => false,
            Ok(PrefsOutcome::NoUser) => {
                warn!("Soloist logged in without a user dir; lossless from its next start");
                false
            }
            Err(e) => {
                warn!("could not set Soloist's quality prefs: {e}");
                false
            }
        }
    }

    /// Stop Soloist (or clean up after it stopped) and tell core it went inactive, so
    /// the next activation restores the saved volume, and that it stopped, so the player
    /// no longer reads Playing. Both after everything this Soloist said.
    async fn stop(&self, backend: SoloistBackend) {
        if let Err(e) = backend.shutdown().await {
            warn!("stopping Soloist: {e}");
        }
        let _ = self.to_core.send(PlaybackEvent::Active(false)).await;
        let stopped = PlaybackEvent::Exited {
            code: None,
            tail: String::new(),
        };
        let _ = self.to_core.send(stopped).await;
    }

    async fn execute(&self, backend: &SoloistBackend, cmd: SoloistCmd, reply: CmdReply) {
        use SoloistCmd as C;
        let result = match cmd {
            C::Play(uri) => backend.play(uri.as_deref()).await,
            C::PlayContext(p) => return self.play_context(backend, p, reply).await,
            C::Pause => backend.pause().await,
            C::Next => backend.next().await,
            C::Prev => backend.prev().await,
            C::Seek(position_ms) => backend.seek(position_ms).await,
            C::Volume(volume) => backend.set_volume(volume).await,
            C::Shuffle(on) => backend.set_shuffle(on).await,
            C::Repeat(mode) => backend.set_repeat(mode).await,
            C::Queue(uri) => backend.add_to_queue(&uri).await,
            C::Activate => backend.activate().await,
            C::Refresh => backend.refresh().await,
            // Handled by `supervise` before it gets here.
            C::Restart => Ok(()),
        };
        let _ = reply.send(result.map_err(|e| e.to_string()));
    }

    /// The Connect play command to Soloist's device, from this run's `from_id`. It goes
    /// out on its own task: an HTTP request must not hold up Soloist's events. A failure
    /// answers in a few words ([`crate::reason`]) and is logged whole. Without a device id
    /// yet, Soloist plays the context from its first track instead.
    async fn play_context(&self, backend: &SoloistBackend, p: ConnectPlay, reply: CmdReply) {
        let data_dir = self.cfg.data_dir.clone();
        let device = tokio::task::spawn_blocking(move || read_device_id(&data_dir))
            .await
            .ok()
            .flatten();
        let Some(to) = device else {
            info!(
                "no Soloist device id yet: playing {} from its first track",
                p.context_uri
            );
            let result = backend.play(Some(&p.context_uri)).await;
            let _ = reply.send(result.map_err(|e| e.to_string()));
            return;
        };
        let connect = self.connect.clone();
        let from = self.from_id.clone();
        tokio::spawn(async move {
            let result = connect.connect_play(&from, &to, &p).await;
            let _ = reply.send(result.map_err(|e| {
                warn!("the Connect play command failed: {e}");
                crate::reason::failure_reason(&e)
            }));
        });
    }

    /// Wait out a restart backoff.
    async fn wait(&mut self, wait: Duration, cmds: &mut Commands) -> Next {
        let over = sleep(wait / self.timings.wait_divisor);
        tokio::pin!(over);
        loop {
            tokio::select! {
                () = &mut over => return Next::Launch,
                cmd = cmds.recv() => {
                    if let Some(next) = self.while_down(cmd) {
                        return next;
                    }
                }
            }
        }
    }

    /// `Failed`, `Expired` or `BinaryMissing`: wait for `Restart`.
    async fn halt(&mut self, cmds: &mut Commands) -> Next {
        loop {
            if let Some(next) = self.while_down(cmds.recv().await) {
                return next;
            }
        }
    }

    /// A command while Soloist isn't running: `Restart` relaunches with a fresh policy,
    /// the rest are refused. `Some` when the loop should move on.
    fn while_down(&mut self, cmd: Option<(SoloistCmd, CmdReply)>) -> Option<Next> {
        match cmd {
            None => Some(Next::Quit),
            Some((SoloistCmd::Restart, reply)) => {
                info!("restarting Soloist on request");
                self.fresh_policy();
                let _ = reply.send(Ok(()));
                Some(Next::Launch)
            }
            Some((_, reply)) => {
                let _ = reply.send(Err(RESTARTING.to_string()));
                None
            }
        }
    }

    fn fresh_policy(&mut self) {
        self.policy = RestartPolicy::new();
        self.attempt = 0;
    }

    fn expired(&self) -> SoloistHealth {
        SoloistHealth::Expired {
            path: self.cfg.binary.display().to_string(),
        }
    }

    fn set_health(&self, health: SoloistHealth) {
        self.health.send_if_modified(|current| {
            if *current == health {
                return false;
            }
            debug!("Soloist health: {health:?}");
            *current = health;
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use futures_util::{SinkExt, StreamExt};
    use playback::{prefs::QUALITY_PREFS, supervisor::running_soloist};
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
    use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};

    use super::*;
    use SoloistHealth::{Expired, Failed, NotPaired, Ready, Reconnecting, Starting};

    /// How long any one expected thing may take.
    const WAIT: Duration = Duration::from_secs(10);
    /// Soloist's user dir, as it creates it at first login.
    const USER_DIR: &str = "settings/Users/0uaktefav92xfmbx26j1narmh-user";
    const CONTEXT: &str = "spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi";
    const TRACK: &str = "spotify:track:4SWyOAtHVNdnczEIsLvMN2";

    fn fake_binary() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../playback/tests/fake-soloist.sh")
    }

    /// Restart waits a tenth as long; every other timing as in the engine.
    fn fast_waits() -> Timings {
        Timings {
            wait_divisor: 10,
            ..Timings::ENGINE
        }
    }

    // ---- A stand-in for Soloist's WebSocket ----

    /// Every launch of the fake binary publishes this server's port, so each launch is one
    /// connection. It answers `get_auth_state` with an `auth_state` frame (`is_active:
    /// true`, so an `Active(false)` reaching core can only come from the loop) and keeps
    /// every command, per connection.
    struct FakeWs {
        port: u16,
        shared: Arc<Shared>,
    }

    #[derive(Default)]
    struct Shared {
        logged_in: AtomicBool,
        /// Made just before the first logged-in `auth_state`: Soloist creates its user dir
        /// at its first login.
        user_dir: Mutex<Option<PathBuf>>,
        /// Commands received, one list per connection.
        commands: Mutex<Vec<Vec<Value>>>,
        /// Frames for each live connection to send.
        push: Mutex<Vec<mpsc::UnboundedSender<Message>>>,
    }

    impl Shared {
        fn auth_frame(&self) -> Message {
            let logged_in = self.logged_in.load(Ordering::SeqCst);
            if logged_in && let Some(dir) = self.user_dir.lock().unwrap().take() {
                std::fs::create_dir_all(dir).unwrap();
            }
            let frame = json!({"type": "auth_state", "logged_in": logged_in, "is_active": true});
            Message::text(frame.to_string())
        }
    }

    impl FakeWs {
        async fn start(logged_in: bool) -> FakeWs {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let shared = Arc::new(Shared::default());
            shared.logged_in.store(logged_in, Ordering::SeqCst);
            let s = shared.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let Ok(ws) = accept_async(stream).await else {
                        continue;
                    };
                    let index = {
                        let mut commands = s.commands.lock().unwrap();
                        commands.push(Vec::new());
                        commands.len() - 1
                    };
                    let (tx, rx) = mpsc::unbounded_channel();
                    s.push.lock().unwrap().push(tx);
                    tokio::spawn(serve(ws, index, s.clone(), rx));
                }
            });
            FakeWs { port, shared }
        }

        /// Soloist finishes logging in, and says so on every live connection.
        fn log_in(&self) {
            self.shared.logged_in.store(true, Ordering::SeqCst);
            for tx in self.shared.push.lock().unwrap().iter() {
                let _ = tx.send(self.shared.auth_frame());
            }
        }

        /// Close every live connection, as when Soloist's WebSocket goes away.
        fn close_all(&self) {
            for tx in self.shared.push.lock().unwrap().drain(..) {
                let _ = tx.send(Message::Close(None));
            }
        }

        fn connections(&self) -> usize {
            self.shared.commands.lock().unwrap().len()
        }

        fn commands(&self, connection: usize) -> Vec<Value> {
            self.shared.commands.lock().unwrap()[connection].clone()
        }

        fn names(&self, connection: usize) -> Vec<String> {
            self.commands(connection)
                .iter()
                .map(|c| c["command"].as_str().unwrap_or_default().to_string())
                .collect()
        }
    }

    async fn serve(
        ws: WebSocketStream<tokio::net::TcpStream>,
        index: usize,
        shared: Arc<Shared>,
        mut push: mpsc::UnboundedReceiver<Message>,
    ) {
        let (mut sink, mut stream) = ws.split();
        loop {
            tokio::select! {
                frame = stream.next() => match frame {
                    Some(Ok(Message::Text(text))) => {
                        let cmd: Value = serde_json::from_str(text.as_str()).unwrap();
                        let auth = cmd["command"] == "get_auth_state";
                        shared.commands.lock().unwrap()[index].push(cmd);
                        if auth && sink.send(shared.auth_frame()).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                },
                msg = push.recv() => match msg {
                    Some(close @ Message::Close(_)) => {
                        let _ = sink.send(close).await;
                        break;
                    }
                    Some(frame) => {
                        if sink.send(frame).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
    }

    // ---- Stubs for the key cache and the Connect command ----

    struct Key;

    #[async_trait]
    impl KeyCache for Key {
        async fn load(&self) -> Option<String> {
            Some("KEY".into())
        }
        async fn save(&self, _: &str) {}
        async fn forget(&self) -> playback::Result<()> {
            Ok(())
        }
    }

    /// Records each `connect_play(from, to, p)`; answers `Err` while `fail` is set, and
    /// a network error while `offline` is.
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<(String, String, ConnectPlay)>>,
        fail: AtomicBool,
        offline: AtomicBool,
    }

    #[async_trait]
    impl ConnectPlayer for Recorder {
        async fn connect_play(
            &self,
            from: &str,
            to: &str,
            p: &ConnectPlay,
        ) -> spot_core::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((from.into(), to.into(), p.clone()));
            if self.fail.load(Ordering::SeqCst) {
                return Err(spot_core::SpotError::Http {
                    status: 404,
                    what: "/connect-state".into(),
                });
            }
            if self.offline.load(Ordering::SeqCst) {
                return Err(spot_core::SpotError::Net(format!(
                    "/connect-state/v1/player/command/from/{from}/to/{to}: error sending \
                     request: client error (Connect): dns error: failed to lookup address \
                     information: Temporary failure in name resolution"
                )));
            }
            Ok(())
        }
    }

    impl Recorder {
        fn calls(&self) -> Vec<(String, String, ConnectPlay)> {
            self.calls.lock().unwrap().clone()
        }
    }

    // ---- The harness ----

    struct Opts {
        mode: &'static str,
        env: Vec<(&'static str, &'static str)>,
        /// The fake WebSocket reports a login from the start.
        logged_in: bool,
        timings: Timings,
        connect: Arc<dyn ConnectPlayer>,
        /// Never paired before: no user dir until the first login creates it.
        first_session: bool,
        /// Written to `.device_id` before the start.
        device_id: Option<&'static str>,
    }

    impl Default for Opts {
        fn default() -> Self {
            Opts {
                mode: "ok",
                env: Vec::new(),
                logged_in: true,
                timings: Timings::ENGINE,
                connect: Arc::new(Recorder::default()),
                first_session: false,
                device_id: None,
            }
        }
    }

    struct Harness {
        dir: tempfile::TempDir,
        ws: FakeWs,
        health: watch::Receiver<SoloistHealth>,
        /// Every health value the recorder task saw, in order.
        seen: Arc<Mutex<Vec<SoloistHealth>>>,
        core: mpsc::Receiver<PlaybackEvent>,
        notices: mpsc::Receiver<String>,
        cmds: mpsc::Sender<(SoloistCmd, CmdReply)>,
        wake: watch::Sender<u64>,
        task: JoinHandle<()>,
    }

    async fn start(o: Opts) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        if !o.first_session {
            std::fs::create_dir_all(data.join(USER_DIR)).unwrap();
        }
        if let Some(id) = o.device_id {
            std::fs::write(data.join(".device_id"), id).unwrap();
        }
        let ws = FakeWs::start(o.logged_in).await;
        if o.first_session {
            *ws.shared.user_dir.lock().unwrap() = Some(data.join(USER_DIR));
        }
        let mut cfg = SoloistConfig::from_vars(|_| None);
        cfg.binary = fake_binary();
        cfg.data_dir = data;
        cfg.cache_dir = dir.path().join("cache");
        cfg.env = [("FAKE_MODE", o.mode), ("FAKE_PORT", &ws.port.to_string())]
            .into_iter()
            .chain(o.env)
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let (to_core, core) = mpsc::channel(256);
        let (health_tx, health) = watch::channel(Starting);
        let seen = record(&health);
        let (cmds, cmds_rx) = mpsc::channel(8);
        let (wake, wake_rx) = watch::channel(0);
        let (notices_tx, notices) = mpsc::channel(8);
        let mut soloist = Soloist::new(
            cfg,
            Arc::new(Key),
            o.connect,
            to_core,
            health_tx,
            wake_rx,
            notices_tx,
        );
        soloist.timings = o.timings;
        let task = tokio::spawn(soloist.run(cmds_rx));
        Harness {
            dir,
            ws,
            health,
            seen,
            core,
            notices,
            cmds,
            wake,
            task,
        }
    }

    /// Keeps every value `health` takes, as far as a task woken on each change sees.
    fn record(health: &watch::Receiver<SoloistHealth>) -> Arc<Mutex<Vec<SoloistHealth>>> {
        let mut rx = health.clone();
        let seen = Arc::new(Mutex::new(vec![rx.borrow_and_update().clone()]));
        let s = seen.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                s.lock().unwrap().push(rx.borrow_and_update().clone());
            }
        });
        seen
    }

    impl Harness {
        fn data(&self) -> PathBuf {
            self.dir.path().join("data")
        }

        /// Waits until health satisfies `f`, and returns it.
        async fn health_is(&mut self, f: impl FnMut(&SoloistHealth) -> bool) -> SoloistHealth {
            timeout(WAIT, self.health.wait_for(f))
                .await
                .expect("health never got there")
                .unwrap()
                .clone()
        }

        async fn send(&self, cmd: SoloistCmd) -> Result<(), String> {
            let (tx, rx) = oneshot::channel();
            self.cmds.send((cmd, tx)).await.unwrap();
            timeout(WAIT, rx).await.expect("no reply").unwrap()
        }

        /// Reads core's events until one satisfies `f`; returns the ones before it.
        async fn core_sees(
            &mut self,
            mut f: impl FnMut(&PlaybackEvent) -> bool,
        ) -> Vec<PlaybackEvent> {
            let mut before = Vec::new();
            loop {
                let ev = timeout(WAIT, self.core.recv())
                    .await
                    .expect("core never saw it")
                    .unwrap();
                if f(&ev) {
                    return before;
                }
                before.push(ev);
            }
        }

        fn pid(&self) -> u32 {
            std::fs::read_to_string(self.data().join("soloist.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }

        fn signal(&self, signal: &str) {
            let ok = std::process::Command::new("kill")
                .args([format!("-{signal}"), self.pid().to_string()])
                .status()
                .unwrap()
                .success();
            assert!(ok, "kill -{signal}");
        }

        fn reconnects(&self) -> Vec<SoloistHealth> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|h| matches!(h, Reconnecting { .. }))
                .cloned()
                .collect()
        }

        /// Closing the command channel stops Soloist; the loop returns and no fake
        /// Soloist is left running on the data dir.
        async fn stop(self) {
            let Harness {
                cmds, task, dir, ..
            } = self;
            drop(cmds);
            timeout(WAIT, task)
                .await
                .expect("the loop did not return")
                .unwrap();
            assert_eq!(running_soloist(&dir.path().join("data")), None);
        }
    }

    fn reconnecting(attempt: u32, retry_in_ms: u64) -> SoloistHealth {
        Reconnecting {
            attempt,
            retry_in_ms,
        }
    }

    fn play() -> ConnectPlay {
        ConnectPlay {
            context_uri: CONTEXT.into(),
            track_uri: Some(TRACK.into()),
            track_uid: None,
            shuffle: false,
        }
    }

    // ---- The tests ----

    #[tokio::test]
    async fn killed_soloist_restarts_paused() {
        let mut h = start(Opts::default()).await;
        h.health_is(|s| *s == Ready).await;
        let first = h.pid();
        h.signal("KILL");
        // Soloist's socket dies with it, a moment before its exit is reported.
        h.ws.close_all();
        let health = h.health_is(|s| matches!(s, Reconnecting { .. })).await;
        assert_eq!(health, reconnecting(1, 1000));
        // Core hears this device went inactive, so the next activation restores the volume.
        let before = h.core_sees(|e| *e == PlaybackEvent::Active(false)).await;
        assert!(
            !before.iter().any(|e| matches!(e, PlaybackEvent::Error(_))),
            "the closed socket is not an error to show: {before:?}"
        );
        // Then that Soloist stopped, after all it said (final review I2): nothing plays.
        let between = h
            .core_sees(|e| matches!(e, PlaybackEvent::Exited { .. }))
            .await;
        assert_eq!(between, [], "right after");

        h.health_is(|s| *s == Ready).await;
        assert_ne!(h.pid(), first, "relaunched");
        assert_eq!(h.ws.connections(), 2);
        // Soloist restores its queue paused; LOUDAUTO only asks for state, never plays.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let sent = h.ws.names(1);
        assert!(
            sent.iter()
                .all(|c| ["get_auth_state", "get_state", "get_queue"].contains(&c.as_str())),
            "{sent:?}"
        );
        assert!(sent.iter().any(|c| c == "get_state"), "{sent:?}");
        h.stop().await;
    }

    #[tokio::test]
    async fn exit_10_is_expired() {
        let path = fake_binary().display().to_string();

        // An expired build exits before it opens its WebSocket.
        let mut h = start(Opts {
            mode: "expired",
            ..Default::default()
        })
        .await;
        let health = h.health_is(|s| matches!(s, Expired { .. })).await;
        assert_eq!(health, Expired { path: path.clone() });
        h.stop().await;

        // A running Soloist that exits 10 mid-session.
        let mut h = start(Opts {
            mode: "crash",
            env: vec![("FAKE_EXIT", "10")],
            ..Default::default()
        })
        .await;
        h.health_is(|s| *s == Ready).await;
        h.signal("USR1");
        let health = h.health_is(|s| matches!(s, Expired { .. })).await;
        assert_eq!(health, Expired { path });
        // Restarting won't help, so it doesn't: longer than the first backoff, no relaunch.
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(matches!(*h.health.borrow(), Expired { .. }));
        assert_eq!(h.ws.connections(), 1);
        assert!(h.reconnects().is_empty(), "{:?}", h.reconnects());
        h.stop().await;
    }

    #[tokio::test]
    async fn five_quick_crashes_fail_with_tail() {
        let mut h = start(Opts {
            mode: "crashloop",
            timings: fast_waits(),
            ..Default::default()
        })
        .await;
        let health = h.health_is(|s| matches!(s, Failed { .. })).await;
        let Failed { message, log_tail } = health else {
            unreachable!()
        };
        assert!(log_tail.contains("boom"), "{log_tail:?}");
        assert!(message.contains("exit code 3"), "{message:?}");
        // The waits shown are the policy's (the test only shortens the real ones).
        assert_eq!(
            h.reconnects(),
            [
                reconnecting(1, 1000),
                reconnecting(2, 2000),
                reconnecting(3, 5000),
                reconnecting(4, 10_000)
            ]
        );
        let launches = std::fs::read_to_string(h.data().join("fake-crashes")).unwrap();
        assert_eq!(launches.trim(), "5", "no sixth launch after giving up");
        h.stop().await;
    }

    #[tokio::test]
    async fn first_session_rewrites_prefs_and_restarts_once() {
        let mut h = start(Opts {
            first_session: true,
            ..Default::default()
        })
        .await;
        h.health_is(|s| *s == Ready).await;
        assert_eq!(
            timeout(WAIT, h.notices.recv()).await.unwrap().as_deref(),
            Some(LOSSLESS_NOTICE)
        );
        assert_eq!(h.ws.connections(), 2, "relaunched once");
        // The first run went straight from Starting to the relaunch, never Ready.
        assert_eq!(*h.seen.lock().unwrap(), [Starting, Ready]);
        let prefs = std::fs::read_to_string(h.data().join(USER_DIR).join("prefs")).unwrap();
        for (k, v) in QUALITY_PREFS {
            assert!(prefs.lines().any(|l| l == format!("{k}={v}")), "{prefs}");
        }

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.ws.connections(), 2, "only once");
        assert!(h.notices.try_recv().is_err(), "one notice");

        // The prefs restart was the loop's own doing, not a failure: a crash now is the
        // first, with the first backoff.
        h.signal("KILL");
        let health = h.health_is(|s| matches!(s, Reconnecting { .. })).await;
        assert_eq!(health, reconnecting(1, 1000));
        h.stop().await;
    }

    #[tokio::test]
    async fn restart_after_give_up() {
        let mut h = start(Opts {
            mode: "crashloop",
            logged_in: false,
            timings: fast_waits(),
            ..Default::default()
        })
        .await;
        h.health_is(|s| matches!(s, Failed { .. })).await;
        assert_eq!(h.send(SoloistCmd::Pause).await, Err(RESTARTING.into()));

        assert_eq!(h.send(SoloistCmd::Restart).await, Ok(()));
        h.health_is(|s| *s == Starting).await;
        h.ws.log_in();
        h.health_is(|s| *s == Ready).await;
        assert_eq!(h.ws.connections(), 1, "the sixth launch started");
        h.stop().await;
    }

    #[tokio::test]
    async fn play_context_uses_connect_with_device_id() {
        let recorder = Arc::new(Recorder::default());
        let mut h = start(Opts {
            connect: recorder.clone(),
            device_id: Some("dev-1\n"),
            ..Default::default()
        })
        .await;
        h.health_is(|s| *s == Ready).await;
        assert_eq!(h.send(SoloistCmd::PlayContext(play())).await, Ok(()));
        assert_eq!(h.send(SoloistCmd::PlayContext(play())).await, Ok(()));

        let calls = recorder.calls();
        assert_eq!(calls.len(), 2);
        let (from, to, p) = &calls[0];
        assert_eq!(to, "dev-1");
        assert_eq!(p, &play());
        assert_eq!(from.len(), 40);
        assert!(from.bytes().all(|b| b.is_ascii_hexdigit()), "{from}");
        assert_eq!(&calls[1].0, from, "one from id per engine run");
        let sent = h.ws.names(0);
        assert!(
            !sent.iter().any(|c| c == "play" || c == "activate"),
            "{sent:?}"
        );

        // A refused command reaches the caller.
        recorder.fail.store(true, Ordering::SeqCst);
        let err = h.send(SoloistCmd::PlayContext(play())).await.unwrap_err();
        assert!(err.contains("404"), "{err}");
        h.stop().await;
    }

    /// The owner's report (2026-11-08): a double-click during a Wi-Fi drop showed
    /// "Couldn't play: network: /connect-state/v1/player/command/from/…" cut short in the
    /// red box. A failed play says why in a few words, like the heart does.
    #[tokio::test]
    async fn a_failed_connect_play_says_why_plainly() {
        let recorder = Arc::new(Recorder::default());
        recorder.offline.store(true, Ordering::SeqCst);
        let mut h = start(Opts {
            connect: recorder.clone(),
            device_id: Some("dev-1\n"),
            ..Default::default()
        })
        .await;
        h.health_is(|s| *s == Ready).await;
        let err = h.send(SoloistCmd::PlayContext(play())).await.unwrap_err();
        assert_eq!(err, "no connection to Spotify");
        h.stop().await;
    }

    #[tokio::test]
    async fn play_context_without_device_id_plays_the_context() {
        let recorder = Arc::new(Recorder::default());
        let mut h = start(Opts {
            connect: recorder.clone(),
            ..Default::default()
        })
        .await;
        h.health_is(|s| *s == Ready).await;
        assert_eq!(h.send(SoloistCmd::PlayContext(play())).await, Ok(()));
        assert!(recorder.calls().is_empty());
        // `activate`, then `play{context_uri}`, from the top.
        let deadline = tokio::time::Instant::now() + WAIT;
        let sent = loop {
            let sent: Vec<Value> =
                h.ws.commands(0)
                    .into_iter()
                    .filter(|c| c["command"] == "activate" || c["command"] == "play")
                    .collect();
            if sent.len() >= 2 || tokio::time::Instant::now() > deadline {
                break sent;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            sent,
            [
                json!({"type": "command", "command": "activate"}),
                json!({"type": "command", "command": "play", "uri": CONTEXT}),
            ]
        );
        h.stop().await;
    }

    #[tokio::test]
    async fn play_uri_activates_then_plays() {
        let mut h = start(Opts::default()).await;
        h.health_is(|s| *s == Ready).await;
        assert_eq!(h.send(SoloistCmd::Play(Some(TRACK.into()))).await, Ok(()));
        assert_eq!(h.send(SoloistCmd::Pause).await, Ok(()));
        let deadline = tokio::time::Instant::now() + WAIT;
        while !h.ws.names(0).iter().any(|c| c == "pause") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{:?}",
                h.ws.names(0)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let sent: Vec<Value> =
            h.ws.commands(0)
                .into_iter()
                .filter(|c| ["activate", "play", "pause"].contains(&c["command"].as_str().unwrap()))
                .collect();
        assert_eq!(
            sent,
            [
                json!({"type": "command", "command": "activate"}),
                json!({"type": "command", "command": "play", "uri": TRACK}),
                json!({"type": "command", "command": "pause"}),
            ]
        );
        h.stop().await;
    }

    #[tokio::test]
    async fn commands_wait_for_ready() {
        let mut h = start(Opts {
            logged_in: false,
            ..Default::default()
        })
        .await;
        // Running, connected, not logged in yet.
        timeout(WAIT, async {
            while h.ws.connections() == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*h.health.borrow(), Starting);
        assert_eq!(h.send(SoloistCmd::Pause).await, Err(RESTARTING.into()));
        h.ws.log_in();
        h.health_is(|s| *s == Ready).await;
        assert_eq!(h.send(SoloistCmd::Pause).await, Ok(()));
        h.stop().await;
    }

    #[tokio::test]
    async fn not_paired_until_it_logs_in() {
        let mut h = start(Opts {
            logged_in: false,
            timings: Timings {
                not_paired_after: Duration::from_millis(300),
                ..Timings::ENGINE
            },
            ..Default::default()
        })
        .await;
        h.health_is(|s| *s == NotPaired).await;
        assert_eq!(h.send(SoloistCmd::Next).await, Err(RESTARTING.into()));
        // Paired from the desktop app: Soloist logs in.
        h.ws.log_in();
        h.health_is(|s| *s == Ready).await;
        assert_eq!(*h.seen.lock().unwrap(), [Starting, NotPaired, Ready]);
        h.stop().await;
    }

    #[tokio::test]
    async fn healthy_run_resets_the_backoff() {
        let mut h = start(Opts {
            timings: Timings {
                healthy_after: Duration::from_millis(800),
                ..fast_waits()
            },
            ..Default::default()
        })
        .await;
        h.health_is(|s| *s == Ready).await;
        h.signal("KILL");
        assert_eq!(
            h.health_is(|s| matches!(s, Reconnecting { .. })).await,
            reconnecting(1, 1000)
        );
        h.health_is(|s| *s == Ready).await;
        // A short run: the backoff goes on.
        h.signal("KILL");
        assert_eq!(
            h.health_is(|s| matches!(s, Reconnecting { .. })).await,
            reconnecting(2, 2000)
        );
        h.health_is(|s| *s == Ready).await;
        // A healthy run: back to the first wait.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        h.signal("KILL");
        assert_eq!(
            h.health_is(|s| matches!(s, Reconnecting { .. })).await,
            reconnecting(1, 1000)
        );
        h.stop().await;
    }

    #[tokio::test]
    async fn closed_socket_without_an_exit_restarts_soloist() {
        let mut h = start(Opts {
            timings: Timings {
                closed_grace: Duration::from_millis(300),
                ..Timings::ENGINE
            },
            ..Default::default()
        })
        .await;
        h.health_is(|s| *s == Ready).await;
        let first = h.pid();
        h.ws.close_all();
        assert_eq!(
            h.health_is(|s| matches!(s, Reconnecting { .. })).await,
            reconnecting(1, 1000)
        );
        let before = h.core_sees(|e| *e == PlaybackEvent::Active(false)).await;
        assert!(
            !before.iter().any(|e| matches!(e, PlaybackEvent::Error(_))),
            "{before:?}"
        );
        h.health_is(|s| *s == Ready).await;
        assert_ne!(h.pid(), first);
        h.stop().await;
    }

    #[tokio::test]
    async fn wake_asks_for_fresh_state() {
        let mut h = start(Opts::default()).await;
        h.health_is(|s| *s == Ready).await;
        let count = |h: &Harness| h.ws.names(0).iter().filter(|c| *c == "get_state").count();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let before = count(&h);
        h.wake.send_modify(|n| *n += 1);
        timeout(WAIT, async {
            while count(&h) == before {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("no get_state after the wake");
        h.stop().await;
    }

    #[tokio::test]
    async fn expiry_reaches_core() {
        // The fake logs `client expires in 89 days` at startup, before `launch` returns.
        let mut h = start(Opts {
            mode: "nonutf8",
            ..Default::default()
        })
        .await;
        h.core_sees(|e| *e == PlaybackEvent::ExpiresInDays(89))
            .await;
        h.stop().await;
    }

    #[tokio::test]
    async fn missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = SoloistConfig::from_vars(|_| None);
        cfg.binary = dir.path().join("soloist");
        cfg.data_dir = dir.path().join("data");
        cfg.cache_dir = dir.path().join("cache");
        let (to_core, _core) = mpsc::channel(8);
        let (health_tx, mut health) = watch::channel(Starting);
        let (cmds, cmds_rx) = mpsc::channel(8);
        let (_wake, wake_rx) = watch::channel(0);
        let (notices, _notices) = mpsc::channel(8);
        let task = tokio::spawn(run_soloist(
            cfg,
            Arc::new(Key),
            Arc::new(Recorder::default()),
            to_core,
            health_tx,
            cmds_rx,
            wake_rx,
            notices,
        ));
        let got = timeout(WAIT, health.wait_for(|s| *s != Starting))
            .await
            .unwrap()
            .unwrap()
            .clone();
        assert_eq!(
            got,
            SoloistHealth::BinaryMissing {
                path: dir.path().join("soloist").display().to_string()
            }
        );
        drop(cmds);
        timeout(WAIT, task).await.unwrap().unwrap();
    }

    #[test]
    fn device_id_is_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_device_id(dir.path()), None);
        std::fs::write(dir.path().join(".device_id"), "  ").unwrap();
        assert_eq!(read_device_id(dir.path()), None);
        std::fs::write(dir.path().join(".device_id"), "abc123\n").unwrap();
        assert_eq!(read_device_id(dir.path()).as_deref(), Some("abc123"));
    }
}
