//! Session supervision: keeps the one librespot session alive, runs the device-code
//! login when no login is stored, and reconnects when something hints that the
//! session died (spec: "The session heals itself", "First run, login, settings").
//!
//! `run_session` is the only writer of the `SessionCell`. The engine creates the cell
//! empty (`SessionCell::empty`), hands it to `Library` and `SessionTokens`, and spawns
//! `run_session` on it; requests made before the first connect fail with `NotLoggedIn`.
//!
//! - Stored login → connect → `Login(LoggedIn)`, `Username`, `Health(Ready)`.
//! - No usable stored login → `Login(Needed)`, then wait for `SessionHint::StartLogin`,
//!   which runs the device-code login: `Login(Prompt{url, code})`, then the three
//!   updates above, or `Login(Failed(reason))` and another wait.
//! - A failed connect, or a session found dead after a failed request →
//!   `Health(Lost{retry_in_ms, reason})`,
//!   wait 1, 2, 5, 10, 30, 30 … s, `Health(Connecting)`, connect again.
//! - A resume from sleep → `Health(Connecting)` and a fresh connect, as above.
//!
//! Event-driven: apart from a backoff wait, nothing runs between hints, and nothing
//! polls the session.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use engine_ipc::{LoginStep, SessionHealth};
use librespot_core::Session;
use log::{debug, info, warn};
use spot_core::{
    Result, SpotError,
    creds::CredentialStore,
    session::{PairPrompt, SessionCell, connect_stored, pair},
};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, sleep, timeout},
};

/// What the supervisor tells the engine, to fill `EngineState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionUpdate {
    Health(SessionHealth),
    Login(LoginStep),
    Username(String),
}

/// What the engine tells the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHint {
    /// The machine resumed from sleep (sent by `sleep::watch_sleep`): always reconnect.
    /// Right after a resume `is_invalid()` is usually still false on a dead socket
    /// (librespot's keep-alive notices minutes later), and a reconnect takes about a
    /// second, once per real suspend.
    Woke,
    /// A Web or Library request failed with a librespot error
    /// (`SpotError::Librespot`): reconnect if the session died.
    RequestFailed,
    /// A request got a 401 that survived Web's own retry (`SpotError::Http{status:
    /// 401, ..}`). librespot caches the login5 token with no way to drop it, so only a
    /// new session heals this: reconnect now, at most once per minute, so an endpoint
    /// that always answers 401 can't cause a reconnect loop.
    AuthRejected,
    /// The window asked to log in (`Request::StartLogin`). Acted on only while the
    /// login is `Needed`; when logged in it just repeats `Login(LoggedIn)`.
    StartLogin,
}

/// Waits before reconnect attempts, in seconds; the last repeats forever.
const BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// `AuthRejected` reconnects at most this often.
const AUTH_RECONNECT_EVERY: Duration = Duration::from_secs(60);
/// One connect with the stored login: a healthy one takes about a second, and a
/// black-holed access point would otherwise hold the supervisor for many minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LOST: &str = "the connection to Spotify was lost";
const CONNECT_TIMED_OUT: &str = "connecting to Spotify timed out";

/// Supervises the session in `cell` for as long as the engine runs, logging in with
/// the login in `store`. Returns once `out` or `hints` closes (the engine is going
/// away).
pub async fn run_session(
    cell: Arc<SessionCell>,
    store: Arc<dyn CredentialStore>,
    out: mpsc::Sender<SessionUpdate>,
    hints: mpsc::Receiver<SessionHint>,
) {
    supervise(&cell, &StoreConnector(store), &out, hints).await;
    debug!("session supervisor stopped: the engine closed its channel");
}

/// Where sessions come from: `StoreConnector` in the engine, a script in the tests.
#[async_trait]
trait Connector: Send + Sync {
    /// Connect with the stored login (`spot_core::session::connect_stored`).
    async fn stored(&self) -> Result<Session>;
    /// A new device-code login (`spot_core::session::pair`). `prompt` gets the URL and
    /// code as soon as Spotify hands them out; the call returns once the user approves.
    async fn pair(&self, prompt: oneshot::Sender<PairPrompt>) -> Result<Session>;
}

struct StoreConnector(Arc<dyn CredentialStore>);

#[async_trait]
impl Connector for StoreConnector {
    async fn stored(&self) -> Result<Session> {
        connect_stored(&*self.0).await
    }

    async fn pair(&self, prompt: oneshot::Sender<PairPrompt>) -> Result<Session> {
        pair(&*self.0, move |p| {
            let _ = prompt.send(p.clone());
        })
        .await
    }
}

enum State {
    /// Connect with the stored login.
    Connect,
    /// Report the loss, wait `delay`, then connect.
    Wait { delay: Duration, reason: String },
    /// No usable stored login: wait for `StartLogin`.
    Login,
    /// `session` is in the cell; watch it.
    Ready(Session),
}

/// The supervisor's loop. Only ever returns `None`, when `out` or `hints` closed.
async fn supervise(
    cell: &SessionCell,
    connector: &dyn Connector,
    out: &mpsc::Sender<SessionUpdate>,
    mut hints: mpsc::Receiver<SessionHint>,
) -> Option<()> {
    let mut backoff = Backoff::default();
    let mut auth = AuthRefresh::default();
    let mut state = State::Connect;
    loop {
        state = match state {
            State::Connect => match timeout(CONNECT_TIMEOUT, connector.stored()).await {
                Ok(Ok(session)) => {
                    backoff.reset();
                    ready(cell, out, session).await?
                }
                Ok(Err(e)) if needs_login(&e) => {
                    info!("no usable stored login ({e}); waiting for a login");
                    send(out, SessionUpdate::Login(LoginStep::Needed)).await?;
                    State::Login
                }
                Ok(Err(e)) => State::Wait {
                    delay: backoff.next(),
                    reason: e.to_string(),
                },
                Err(_) => State::Wait {
                    delay: backoff.next(),
                    reason: CONNECT_TIMED_OUT.into(),
                },
            },
            State::Wait { delay, reason } => {
                warn!("session: {reason}; reconnecting in {delay:?}");
                let retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
                let lost = SessionHealth::Lost {
                    retry_in_ms,
                    reason,
                };
                send(out, SessionUpdate::Health(lost)).await?;
                let retry = sleep(delay);
                tokio::pin!(retry);
                loop {
                    tokio::select! {
                        () = &mut retry => break,
                        // A reconnect is already on its way; nothing to add.
                        hint = hints.recv() => debug!("session: {:?} while reconnecting", hint?),
                    }
                }
                send(out, SessionUpdate::Health(SessionHealth::Connecting)).await?;
                State::Connect
            }
            State::Login => match hints.recv().await? {
                SessionHint::StartLogin => match start_login(connector, out).await? {
                    Ok(session) => {
                        backoff.reset();
                        ready(cell, out, session).await?
                    }
                    Err(_) => State::Login,
                },
                hint => {
                    debug!("session: {hint:?} while logged out");
                    State::Login
                }
            },
            State::Ready(session) => {
                watch(&session, out, &mut hints, &mut backoff, &mut auth).await?
            }
        };
    }
}

/// Watches a ready session until it needs replacing: found dead after a failed
/// request (→ `Wait`), a resume from sleep (→ `Connect`), or a 401 it can't heal
/// (→ `Connect`, while the old session keeps serving).
async fn watch(
    session: &Session,
    out: &mpsc::Sender<SessionUpdate>,
    hints: &mut mpsc::Receiver<SessionHint>,
    backoff: &mut Backoff,
    auth: &mut AuthRefresh,
) -> Option<State> {
    loop {
        match hints.recv().await? {
            SessionHint::Woke => {
                info!("session: reconnecting after a resume from sleep");
                send(out, SessionUpdate::Health(SessionHealth::Connecting)).await?;
                return Some(State::Connect);
            }
            SessionHint::RequestFailed => {
                if session.is_invalid() {
                    return Some(State::Wait {
                        delay: backoff.next(),
                        reason: LOST.into(),
                    });
                }
            }
            SessionHint::AuthRejected => {
                if auth.allow(Instant::now()) {
                    info!("session: a 401 survived a token refresh; reconnecting for a new token");
                    return Some(State::Connect);
                }
                debug!("session: 401 ignored; the last reconnect for one was under a minute ago");
            }
            SessionHint::StartLogin => {
                send(out, SessionUpdate::Login(LoginStep::LoggedIn)).await?;
            }
        }
    }
}

/// Puts a fresh session in the cell, then tells the engine.
async fn ready(
    cell: &SessionCell,
    out: &mpsc::Sender<SessionUpdate>,
    session: Session,
) -> Option<State> {
    info!("session ready");
    cell.replace(session.clone());
    send(out, SessionUpdate::Login(LoginStep::LoggedIn)).await?;
    send(out, SessionUpdate::Username(session.username())).await?;
    send(out, SessionUpdate::Health(SessionHealth::Ready)).await?;
    Some(State::Ready(session))
}

/// The device-code login: `Login(Prompt{url, code})` as soon as Spotify hands out a
/// code, then the new session, or `Login(Failed(reason))`. `None` when `out` closed.
async fn start_login(
    connector: &dyn Connector,
    out: &mpsc::Sender<SessionUpdate>,
) -> Option<Result<Session>> {
    info!("starting the device-code login");
    let (prompt_tx, mut prompt_rx) = oneshot::channel();
    let pairing = connector.pair(prompt_tx);
    tokio::pin!(pairing);
    let mut prompted = false;
    let result = loop {
        tokio::select! {
            biased;
            prompt = &mut prompt_rx, if !prompted => {
                prompted = true;
                if let Ok(prompt) = prompt {
                    send_prompt(out, prompt).await?;
                }
            }
            result = &mut pairing => break result,
        }
    };
    // A pairing can hand out its code and finish within one poll.
    if !prompted && let Ok(prompt) = prompt_rx.try_recv() {
        send_prompt(out, prompt).await?;
    }
    if let Err(e) = &result {
        warn!("login failed: {e}");
        send(out, SessionUpdate::Login(LoginStep::Failed(e.to_string()))).await?;
    }
    Some(result)
}

async fn send_prompt(out: &mpsc::Sender<SessionUpdate>, prompt: PairPrompt) -> Option<()> {
    let PairPrompt { url, code } = prompt;
    send(out, SessionUpdate::Login(LoginStep::Prompt { url, code })).await
}

async fn send(out: &mpsc::Sender<SessionUpdate>, update: SessionUpdate) -> Option<()> {
    out.send(update).await.ok()
}

/// The stored login can't be used at all, so retrying won't help: the user must log
/// in again. (`connect_stored` has already cleared a corrupt or rejected one.)
fn needs_login(e: &SpotError) -> bool {
    matches!(
        e,
        SpotError::NotLoggedIn
            | SpotError::CorruptCredentials
            | SpotError::StoredCredentialsRejected
    )
}

/// The waits between reconnect attempts: 1, 2, 5, 10, 30, 30, … s.
#[derive(Default)]
struct Backoff {
    step: usize,
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let secs = BACKOFF_SECS[self.step.min(BACKOFF_SECS.len() - 1)];
        self.step = self.step.saturating_add(1);
        Duration::from_secs(secs)
    }

    fn reset(&mut self) {
        self.step = 0;
    }
}

/// Allows an `AuthRejected` reconnect at most once per `AUTH_RECONNECT_EVERY`.
#[derive(Default)]
struct AuthRefresh {
    last: Option<Instant>,
}

impl AuthRefresh {
    fn allow(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < AUTH_RECONNECT_EVERY)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use librespot_core::SessionConfig;
    use spot_core::creds::FileStore;
    use tokio::{sync::Notify, time::timeout};

    use super::*;
    use LoginStep::{Failed, LoggedIn, Needed, Prompt};
    use SessionUpdate::{Health, Login, Username};

    // --- pure helpers ---

    #[test]
    fn backoff_steps() {
        let mut backoff = Backoff::default();
        let secs: Vec<u64> = (0..6).map(|_| backoff.next().as_secs()).collect();
        assert_eq!(secs, [1, 2, 5, 10, 30, 30]);
    }

    #[test]
    fn reset_returns_to_one() {
        let mut backoff = Backoff::default();
        backoff.next();
        backoff.next();
        backoff.next();
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_secs(1));
        assert_eq!(backoff.next(), Duration::from_secs(2));
    }

    #[test]
    fn auth_reconnect_at_most_once_a_minute() {
        let t0 = Instant::now();
        let mut auth = AuthRefresh::default();
        assert!(auth.allow(t0));
        assert!(!auth.allow(t0 + Duration::from_secs(1)));
        assert!(!auth.allow(t0 + Duration::from_secs(59)));
        assert!(auth.allow(t0 + Duration::from_secs(60)));
        // The window restarts at the reconnect it allowed.
        assert!(!auth.allow(t0 + Duration::from_secs(119)));
        assert!(auth.allow(t0 + Duration::from_secs(120)));
    }

    // --- the real connector (no network: these never reach a connect) ---

    fn file_store(dir: &tempfile::TempDir) -> Arc<FileStore> {
        Arc::new(FileStore {
            path: dir.path().join("creds.json"),
        })
    }

    async fn first_update(store: Arc<FileStore>) -> SessionUpdate {
        let cell = Arc::new(SessionCell::empty());
        let (out, mut updates) = mpsc::channel(8);
        let (_hints, hints_rx) = mpsc::channel(8);
        tokio::spawn(run_session(cell, store, out, hints_rx));
        timeout(Duration::from_secs(10), updates.recv())
            .await
            .expect("no update")
            .expect("supervisor stopped")
    }

    #[tokio::test]
    async fn no_credentials_means_login_needed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(first_update(file_store(&dir)).await, Login(Needed));
    }

    #[tokio::test]
    async fn corrupt_credentials_mean_login_needed() {
        let dir = tempfile::tempdir().unwrap();
        let store = file_store(&dir);
        std::fs::write(&store.path, b"{not json").unwrap();
        assert_eq!(first_update(Arc::clone(&store)).await, Login(Needed));
        assert_eq!(
            store.load().await.unwrap(),
            None,
            "the corrupt login is cleared"
        );
    }

    // --- the state machine, over a scripted connector ---

    enum Step {
        Ok(Session),
        Err(SpotError),
        Hang,
    }

    impl Step {
        async fn run(self) -> Result<Session> {
            match self {
                Step::Ok(session) => Ok(session),
                Step::Err(e) => Err(e),
                Step::Hang => std::future::pending().await,
            }
        }
    }

    #[derive(Default)]
    struct Fake {
        stored: Mutex<VecDeque<Step>>,
        pairs: Mutex<VecDeque<Step>>,
        /// When set, each pairing waits here after handing out its code, the way the
        /// real one waits for the user to approve.
        release: Option<Arc<Notify>>,
        pair_calls: AtomicUsize,
    }

    impl Fake {
        fn stored(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                stored: Mutex::new(steps.into_iter().collect()),
                ..Self::default()
            }
        }

        fn pairs(mut self, steps: impl IntoIterator<Item = Step>) -> Self {
            self.pairs = Mutex::new(steps.into_iter().collect());
            self
        }
    }

    #[async_trait]
    impl Connector for Fake {
        async fn stored(&self) -> Result<Session> {
            let step = self.stored.lock().unwrap().pop_front();
            step.expect("unexpected connect").run().await
        }

        async fn pair(&self, prompt: oneshot::Sender<PairPrompt>) -> Result<Session> {
            let n = self.pair_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = prompt.send(pair_prompt(n));
            if let Some(release) = &self.release {
                release.notified().await;
            }
            let step = self.pairs.lock().unwrap().pop_front();
            step.expect("unexpected pairing").run().await
        }
    }

    fn pair_prompt(n: usize) -> PairPrompt {
        PairPrompt {
            url: "https://www.spotify.com/pair".into(),
            code: format!("CODE-{n}"),
        }
    }

    fn prompt(n: usize) -> SessionUpdate {
        let PairPrompt { url, code } = pair_prompt(n);
        Login(Prompt { url, code })
    }

    fn session(username: &str) -> Session {
        let session = Session::new(SessionConfig::default(), None);
        session.set_username(username);
        session
    }

    fn net_down() -> SpotError {
        SpotError::Net("network is unreachable".into())
    }

    fn ready(username: &str) -> [SessionUpdate; 3] {
        [
            Login(LoggedIn),
            Username(username.into()),
            Health(SessionHealth::Ready),
        ]
    }

    fn lost(ms: u64, reason: &str) -> SessionUpdate {
        Health(SessionHealth::Lost {
            retry_in_ms: ms,
            reason: reason.into(),
        })
    }

    struct Harness {
        cell: Arc<SessionCell>,
        updates: mpsc::Receiver<SessionUpdate>,
        hints: mpsc::Sender<SessionHint>,
    }

    fn start(fake: Fake) -> Harness {
        let cell = Arc::new(SessionCell::empty());
        let (out, updates) = mpsc::channel(64);
        let (hints, hints_rx) = mpsc::channel(16);
        let supervised = Arc::clone(&cell);
        tokio::spawn(async move { supervise(&supervised, &fake, &out, hints_rx).await });
        Harness {
            cell,
            updates,
            hints,
        }
    }

    impl Harness {
        async fn next(&mut self) -> SessionUpdate {
            timeout(Duration::from_secs(600), self.updates.recv())
                .await
                .expect("no update")
                .expect("supervisor stopped")
        }

        async fn expect(&mut self, want: &[SessionUpdate]) {
            for want in want {
                assert_eq!(&self.next().await, want);
            }
        }

        /// Nothing more arrives within a (virtual) second.
        async fn quiet(&mut self) {
            if let Ok(update) = timeout(Duration::from_secs(1), self.updates.recv()).await {
                panic!("unexpected update {update:?}");
            }
        }

        async fn hint(&self, hint: SessionHint) {
            self.hints.send(hint).await.unwrap();
        }

        fn session_id(&self) -> String {
            self.cell.get().unwrap().session_id()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stored_login_connects_and_fills_the_cell() {
        let alice = session("alice");
        let id = alice.session_id();
        let mut h = start(Fake::stored([Step::Ok(alice)]));
        h.expect(&ready("alice")).await;
        assert_eq!(h.session_id(), id);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_connects_back_off_then_ready() {
        let t0 = Instant::now();
        let mut h = start(Fake::stored([
            Step::Err(net_down()),
            Step::Err(net_down()),
            Step::Err(net_down()),
            Step::Ok(session("alice")),
        ]));
        let reason = net_down().to_string();
        h.expect(&[
            lost(1000, &reason),
            Health(SessionHealth::Connecting),
            lost(2000, &reason),
            Health(SessionHealth::Connecting),
            lost(5000, &reason),
            Health(SessionHealth::Connecting),
        ])
        .await;
        h.expect(&ready("alice")).await;
        assert_eq!(Instant::now() - t0, Duration::from_secs(1 + 2 + 5));
    }

    #[tokio::test(start_paused = true)]
    async fn hung_connect_times_out_and_retries() {
        let mut h = start(Fake::stored([Step::Hang, Step::Ok(session("alice"))]));
        h.expect(&[
            lost(1000, CONNECT_TIMED_OUT),
            Health(SessionHealth::Connecting),
        ])
        .await;
        h.expect(&ready("alice")).await;
    }

    #[tokio::test(start_paused = true)]
    async fn wake_reconnects_even_a_live_session() {
        let alice = session("alice");
        let first = alice.session_id();
        let mut h = start(Fake::stored([Step::Ok(alice), Step::Ok(session("alice"))]));
        h.expect(&ready("alice")).await;

        // Right after a resume the old session usually still looks valid (librespot's
        // keep-alive notices a dead socket minutes later), so a wake always reconnects.
        assert!(!h.cell.get().unwrap().is_invalid());
        h.hint(SessionHint::Woke).await;
        h.expect(&[Health(SessionHealth::Connecting)]).await;
        h.expect(&ready("alice")).await;
        assert_ne!(h.session_id(), first, "the cell holds the new session");
    }

    #[tokio::test(start_paused = true)]
    async fn failed_wake_reconnect_backs_off() {
        let mut h = start(Fake::stored([
            Step::Ok(session("alice")),
            Step::Err(net_down()),
            Step::Ok(session("alice")),
        ]));
        h.expect(&ready("alice")).await;

        h.hint(SessionHint::Woke).await;
        h.expect(&[
            Health(SessionHealth::Connecting),
            lost(1000, &net_down().to_string()),
            Health(SessionHealth::Connecting),
        ])
        .await;
        h.expect(&ready("alice")).await;
    }

    #[tokio::test(start_paused = true)]
    async fn lost_session_reconnects_after_a_failed_request() {
        let net = net_down().to_string();
        let mut h = start(Fake::stored([
            Step::Err(net_down()),
            Step::Ok(session("alice")),
            Step::Err(net_down()),
            Step::Ok(session("alice")),
        ]));
        h.expect(&[lost(1000, &net), Health(SessionHealth::Connecting)])
            .await;
        h.expect(&ready("alice")).await;

        h.cell.get().unwrap().shutdown();
        h.hint(SessionHint::RequestFailed).await;
        // The successful connect reset the backoff, so this loss starts at 1 s again.
        h.expect(&[
            lost(1000, LOST),
            Health(SessionHealth::Connecting),
            lost(2000, &net),
            Health(SessionHealth::Connecting),
        ])
        .await;
        h.expect(&ready("alice")).await;
    }

    #[tokio::test(start_paused = true)]
    async fn idle_session_is_not_polled() {
        // Event-driven: a session that dies with no hint is left alone until the next
        // failed request or wake.
        let mut h = start(Fake::stored([Step::Ok(session("alice"))]));
        h.expect(&ready("alice")).await;

        h.cell.get().unwrap().shutdown();
        if let Ok(update) = timeout(Duration::from_secs(3600), h.updates.recv()).await {
            panic!("unexpected update {update:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn live_session_ignores_failed_requests_and_logins() {
        let alice = session("alice");
        let id = alice.session_id();
        let mut h = start(Fake::stored([Step::Ok(alice)]));
        h.expect(&ready("alice")).await;

        h.hint(SessionHint::RequestFailed).await;
        h.quiet().await;
        assert_eq!(h.session_id(), id);

        // A login request while logged in just says so.
        h.hint(SessionHint::StartLogin).await;
        h.expect(&[Login(LoggedIn)]).await;
    }

    #[tokio::test(start_paused = true)]
    async fn auth_rejected_reconnects_at_most_once_a_minute() {
        let alice = session("alice");
        let first = alice.session_id();
        let mut h = start(Fake::stored([
            Step::Ok(alice),
            Step::Ok(session("alice")),
            Step::Ok(session("alice")),
        ]));
        h.expect(&ready("alice")).await;

        // Reconnects at once, with no Lost in between: the old session keeps serving.
        h.hint(SessionHint::AuthRejected).await;
        h.expect(&ready("alice")).await;
        let second = h.session_id();
        assert_ne!(second, first);

        // More 401s inside the minute are ignored.
        h.hint(SessionHint::AuthRejected).await;
        h.hint(SessionHint::AuthRejected).await;
        h.quiet().await;
        assert_eq!(h.session_id(), second);

        tokio::time::advance(AUTH_RECONNECT_EVERY).await;
        h.hint(SessionHint::AuthRejected).await;
        h.expect(&ready("alice")).await;
        assert_ne!(h.session_id(), second);
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_stored_login_waits_for_login() {
        let bob = session("bob");
        let id = bob.session_id();
        let mut h = start(
            Fake::stored([Step::Err(SpotError::StoredCredentialsRejected)]).pairs([Step::Ok(bob)]),
        );
        h.expect(&[Login(Needed)]).await;
        // Nothing but a login request moves it.
        h.hint(SessionHint::Woke).await;
        h.hint(SessionHint::AuthRejected).await;
        h.quiet().await;
        assert!(matches!(h.cell.get(), Err(SpotError::NotLoggedIn)));

        h.hint(SessionHint::StartLogin).await;
        h.expect(&[prompt(1)]).await;
        h.expect(&ready("bob")).await;
        assert_eq!(h.session_id(), id);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_login_says_why_and_can_be_retried() {
        let release = Arc::new(Notify::new());
        let mut h = start(Fake {
            release: Some(Arc::clone(&release)),
            ..Fake::stored([Step::Err(SpotError::NotLoggedIn)]).pairs([
                Step::Err(SpotError::Pairing("the code expired".into())),
                Step::Ok(session("bob")),
            ])
        });
        h.expect(&[Login(Needed)]).await;

        h.hint(SessionHint::StartLogin).await;
        // The code shows while the pairing still waits for the user.
        h.expect(&[prompt(1)]).await;
        h.quiet().await;
        release.notify_one();
        h.expect(&[Login(Failed("pairing failed: the code expired".into()))])
            .await;

        h.hint(SessionHint::StartLogin).await;
        h.expect(&[prompt(2)]).await;
        release.notify_one();
        h.expect(&ready("bob")).await;
    }
}
