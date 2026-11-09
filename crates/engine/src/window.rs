//! Opening the window, or bringing it to the front, for the tray. On Wayland a window
//! can't raise itself, so "raise" is closing the open window (`WindowControl`, over its
//! connection) and starting a fresh one with the activation token the tray was handed:
//! KWin focuses a new window that brings one.
//!
//! - One at a time: a click while another is still opening the window does nothing.
//! - There is only ever one window. One that won't close in time is stopped, if the
//!   launcher started it (SIGTERM, then SIGKILL); one it started that is still coming up
//!   is left to come up.
//! - The window is `loudauto` beside the engine's own binary, else `loudauto` on `$PATH`.
//!   It starts in a scope of its own (`systemd-run --user --scope`) when there is
//!   `systemd-run`, so it outlives the engine's service restarting or stopping; else
//!   directly, as the engine's child.
//! - Each window started is waited on, on its own task, so none is left a zombie.

use std::{
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex as StdMutex, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use engine_ipc::APP_ID;
use log::{debug, info, warn};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use tokio::{
    process::{Child, Command},
    sync::{Mutex, mpsc, watch},
    time::timeout,
};

use crate::server::WindowControl;

/// The window's binary.
pub const WINDOW_BINARY: &str = "loudauto";
/// How long the open window gets to close before it is stopped, or a fresh one starts
/// beside it.
pub const CLOSE_WAIT: Duration = Duration::from_millis(1500);
/// How long a window being stopped gets after SIGTERM before SIGKILL, and after SIGKILL.
pub const STOP_GRACE: Duration = Duration::from_secs(1);
/// How long a window the launcher started may take to connect before a click counts it
/// as stuck rather than still coming up.
pub const START_GRACE: Duration = Duration::from_secs(5);
/// Where a Wayland app finds the activation token it was started with.
pub const ACTIVATION_TOKEN_VAR: &str = "XDG_ACTIVATION_TOKEN";
/// What starts the window in a scope of its own.
pub const SYSTEMD_RUN: &str = "systemd-run";

/// Why `open_or_raise` started no window.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("the window is already being opened")]
    Busy,
    #[error("the window is still starting")]
    Starting,
    #[error("couldn't start the window ({path}): {source}")]
    Spawn { path: PathBuf, source: io::Error },
}

/// How the window is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// As the engine's child, in the engine's cgroup: stopping the engine's service stops
    /// the window too.
    Direct,
    /// Through `systemd-run --user --scope`, which execs the window in place (same pid,
    /// same environment) inside a scope of its own.
    Scope { systemd_run: PathBuf },
}

impl Launch {
    /// `Scope` when `systemd-run` is on `$PATH`, else `Direct`.
    pub fn detect() -> Launch {
        match find_on_path(OsStr::new(SYSTEMD_RUN), std::env::var_os("PATH").as_deref()) {
            Some(systemd_run) => Launch::Scope { systemd_run },
            None => Launch::Direct,
        }
    }
}

/// The program and arguments that start `binary` the `launch` way. `unique` tells this
/// window's scope, `app-to.skunk.loudauto-<unique>.scope`, from earlier ones.
pub fn window_command(launch: &Launch, binary: &Path, unique: &str) -> (PathBuf, Vec<OsString>) {
    match launch {
        Launch::Direct => (binary.to_path_buf(), Vec::new()),
        Launch::Scope { systemd_run } => {
            let unit = format!("--unit=app-{APP_ID}-{unique}.scope");
            let mut args: Vec<OsString> = [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                unit.as_str(),
                "--",
            ]
            .into_iter()
            .map(OsString::from)
            .collect();
            args.push(binary.into());
            (systemd_run.clone(), args)
        }
    }
}

/// The first `<dir>/<name>` that is a file, for the dirs of `path` (a `$PATH` value).
pub fn find_on_path(name: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Starts the window, closing the open one first.
pub struct WindowLauncher {
    windows: Arc<dyn WindowControl>,
    binary: PathBuf,
    launch: Launch,
    /// `START_GRACE`; shorter in tests.
    start_grace: Duration,
    /// The last window this launcher started, while it may still run. Held while an
    /// `open_or_raise` runs.
    own: Mutex<Option<Running>>,
    /// Filled by each window's waiting task once the window has exited.
    last_exit: Arc<StdMutex<Option<ExitStatus>>>,
}

impl WindowLauncher {
    /// Starts `binary` as the window, the `launch` way; `windows` tells whether one is
    /// open, and closes it.
    pub fn new(windows: Arc<dyn WindowControl>, binary: PathBuf, launch: Launch) -> Self {
        WindowLauncher {
            windows,
            binary,
            launch,
            start_grace: START_GRACE,
            own: Mutex::new(None),
            last_exit: Arc::default(),
        }
    }

    /// Opens the window, or brings it to the front, then returns: `Busy` while another
    /// call is still at it. Failures are logged here, so a caller may drop them.
    ///
    /// - A connected window is asked to close (waiting up to `CLOSE_WAIT`). If it doesn't
    ///   and the launcher started it, it is stopped; one the launcher didn't start can't
    ///   be, and the fresh one opens beside it.
    /// - With no window connected, one the launcher started under `START_GRACE` ago is
    ///   still coming up: `Starting`, and nothing else happens. One older than that is
    ///   stuck, and stopped.
    /// - The fresh window gets `XDG_ACTIVATION_TOKEN=<token>` when there is a token.
    pub async fn open_or_raise(&self, token: Option<String>) -> Result<(), OpenError> {
        let Ok(mut own) = self.own.try_lock() else {
            debug!("the window is already being opened; ignoring this click");
            return Err(OpenError::Busy);
        };
        if own.as_ref().is_some_and(Running::ended) {
            *own = None;
        }
        if self.windows.window_connected() {
            if !self.windows.close_window(CLOSE_WAIT).await {
                match own.take() {
                    Some(running) if !running.ended() => {
                        warn!(
                            "the window (pid {}) didn't close within {} ms; stopping it",
                            running.pid,
                            CLOSE_WAIT.as_millis()
                        );
                        running.stop().await;
                    }
                    _ => warn!(
                        "a window LOUDAUTO didn't start didn't close within {} ms and can't \
                         be stopped from here; opening another beside it",
                        CLOSE_WAIT.as_millis()
                    ),
                }
            }
        } else if let Some(running) = own.take() {
            if running.started.elapsed() < self.start_grace {
                debug!(
                    "the window (pid {}) is still starting; leaving it to come up",
                    running.pid
                );
                *own = Some(running);
                return Err(OpenError::Starting);
            }
            warn!(
                "the window (pid {}) has run {} s without connecting; stopping it",
                running.pid,
                running.started.elapsed().as_secs()
            );
            running.stop().await;
        }
        *own = Some(self.spawn(token)?);
        Ok(())
    }

    /// Starts a fresh window, and the task that waits for it.
    fn spawn(&self, token: Option<String>) -> Result<Running, OpenError> {
        let failed = |source: io::Error| {
            let e = OpenError::Spawn {
                path: self.binary.clone(),
                source,
            };
            warn!("{e}");
            e
        };
        // Found here, so a missing window never starts a scope for nothing.
        let binary =
            resolve(&self.binary, std::env::var_os("PATH").as_deref()).ok_or_else(|| {
                failed(io::Error::new(
                    io::ErrorKind::NotFound,
                    "not found beside the engine or on $PATH",
                ))
            })?;
        let (program, args) = window_command(&self.launch, &binary, &unique());
        let mut command = Command::new(program);
        command.args(args).stdin(Stdio::null());
        // Without a token of its own, none: one the engine was started with is spent.
        match &token {
            Some(token) => command.env(ACTIVATION_TOKEN_VAR, token),
            None => command.env_remove(ACTIVATION_TOKEN_VAR),
        };
        let child = command.spawn().map_err(failed)?;
        let pid = child.id().unwrap_or_default();
        info!(
            "started the window (pid {pid}, {}, {} activation token)",
            match self.launch {
                Launch::Direct => "as the engine's child",
                Launch::Scope { .. } => "in a scope of its own",
            },
            if token.is_some() {
                "with an"
            } else {
                "without an"
            }
        );
        let (signals, signals_rx) = mpsc::unbounded_channel();
        let (ended_tx, ended) = watch::channel(false);
        tokio::spawn(own_window(
            child,
            signals_rx,
            ended_tx,
            self.last_exit.clone(),
        ));
        Ok(Running {
            pid,
            started: Instant::now(),
            signals,
            ended,
        })
    }

    /// How the last window to exit exited; `None` until one has.
    pub fn last_exit(&self) -> Option<ExitStatus> {
        *self
            .last_exit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A window the launcher started.
struct Running {
    pid: u32,
    started: Instant,
    /// To its waiting task, which signals it.
    signals: mpsc::UnboundedSender<Signal>,
    /// `true` once it has exited and been reaped.
    ended: watch::Receiver<bool>,
}

impl Running {
    fn ended(&self) -> bool {
        *self.ended.borrow()
    }

    /// Whether it ended within `limit`. Its waiting task gone counts as ended.
    async fn ended_within(&mut self, limit: Duration) -> bool {
        timeout(limit, self.ended.wait_for(|ended| *ended))
            .await
            .is_ok()
    }

    /// SIGTERM, then SIGKILL after `STOP_GRACE`; returns once it has ended, or
    /// `STOP_GRACE` after the SIGKILL.
    async fn stop(mut self) {
        let _ = self.signals.send(Signal::SIGTERM);
        if self.ended_within(STOP_GRACE).await {
            return;
        }
        warn!(
            "the window (pid {}) didn't exit within {} ms of SIGTERM; killing it",
            self.pid,
            STOP_GRACE.as_millis()
        );
        let _ = self.signals.send(Signal::SIGKILL);
        if !self.ended_within(STOP_GRACE).await {
            warn!(
                "the window (pid {}) is still running after SIGKILL",
                self.pid
            );
        }
    }
}

/// Waits for the window `child` to exit, delivering `signals` meanwhile, then records how
/// it exited and says it `ended`. Once the launcher drops its end of `signals` (it started
/// a fresh window), this one is only waited for.
async fn own_window(
    mut child: Child,
    mut signals: mpsc::UnboundedReceiver<Signal>,
    ended: watch::Sender<bool>,
    last_exit: Arc<StdMutex<Option<ExitStatus>>>,
) {
    let mut listening = true;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            signal = signals.recv(), if listening => match signal {
                // `id()` is None once the child is reaped, so a reused pid is never hit.
                Some(signal) => {
                    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
                        let _ = kill(Pid::from_raw(pid), signal);
                    }
                }
                None => listening = false,
            },
        }
    };
    match status {
        Ok(status) => {
            if status.success() {
                debug!("the window exited");
            } else {
                info!("the window exited: {status}");
            }
            *last_exit.lock().unwrap_or_else(PoisonError::into_inner) = Some(status);
        }
        Err(e) => warn!("couldn't wait for the window: {e}"),
    }
    let _ = ended.send(true);
}

/// `binary` if it is a path to a file; a bare name, from `path` (a `$PATH` value).
fn resolve(binary: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    if binary.components().count() > 1 {
        binary.is_file().then(|| binary.to_path_buf())
    } else {
        find_on_path(binary.as_os_str(), path)
    }
}

/// A scope name part no earlier window has: the time, in hex nanoseconds.
fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

/// `loudauto` beside `exe` (the engine's binary) when there is one, else `loudauto`, for
/// `$PATH`.
pub fn window_binary(exe: Option<&Path>) -> PathBuf {
    exe.and_then(Path::parent)
        .map(|dir| dir.join(WINDOW_BINARY))
        .filter(|sibling| sibling.is_file())
        .unwrap_or_else(|| PathBuf::from(WINDOW_BINARY))
}

/// `window_binary` for the running engine.
pub fn default_window_binary() -> PathBuf {
    window_binary(std::env::current_exe().ok().as_deref())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use super::*;

    /// How long a test waits for a window to run, when the brief names no limit: roomy,
    /// for a loaded machine.
    const LIMIT: Duration = Duration::from_secs(5);

    /// The server, as far as the launcher sees it: whether a window is connected, and a
    /// record of each `close_window`.
    #[derive(Default)]
    struct FakeWindows {
        connected: AtomicBool,
        /// The connected window ignores `close_window`, which then times out.
        stays_open: AtomicBool,
        closes: StdMutex<Vec<Duration>>,
        /// Whether the window had already run when `close_window` was asked, per close.
        ran_at_close: StdMutex<Vec<bool>>,
        /// Where the fake window records each run, to look at during a close.
        ran: Option<PathBuf>,
        /// Holds `close_window` until notified, when set.
        hold: Option<Arc<Notify>>,
        held: AtomicUsize,
    }

    impl FakeWindows {
        /// A window is connected, and ignores being asked to close.
        fn stuck() -> FakeWindows {
            FakeWindows {
                connected: AtomicBool::new(true),
                stays_open: AtomicBool::new(true),
                ..FakeWindows::default()
            }
        }
    }

    #[async_trait]
    impl WindowControl for FakeWindows {
        fn window_connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }

        async fn close_window(&self, wait: Duration) -> bool {
            self.closes.lock().unwrap().push(wait);
            if let Some(hold) = &self.hold {
                self.held.fetch_add(1, Ordering::SeqCst);
                hold.notified().await;
            }
            // Long enough for a window started before the close to have run.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let ran = self.ran.as_ref().is_some_and(|ran| ran.exists());
            self.ran_at_close.lock().unwrap().push(ran);
            if self.stays_open.load(Ordering::SeqCst) {
                return false;
            }
            self.connected.store(false, Ordering::SeqCst);
            true
        }
    }

    /// `tests/fake-window.sh`, through a link in a tempdir: it records each run there.
    struct FakeWindow {
        dir: tempfile::TempDir,
        link: PathBuf,
    }

    impl FakeWindow {
        fn new() -> FakeWindow {
            let dir = tempfile::tempdir().unwrap();
            let link = dir.path().join(WINDOW_BINARY);
            let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-window.sh");
            std::os::unix::fs::symlink(script, &link).unwrap();
            FakeWindow { dir, link }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn lines(&self, name: &str) -> Vec<String> {
            std::fs::read_to_string(self.path(name))
                .unwrap_or_default()
                .lines()
                .map(String::from)
                .collect()
        }

        /// The token of each run, `<none>` without one.
        fn runs(&self) -> Vec<String> {
            self.lines("ran")
        }

        /// The signals the `stay` window noted.
        fn signals(&self) -> Vec<String> {
            self.lines("signals")
        }

        /// The pid of each run.
        fn pids(&self) -> Vec<u32> {
            self.lines("pids")
                .iter()
                .map(|pid| pid.parse().unwrap())
                .collect()
        }

        /// The windows started from now keep running (`stay`, `stubborn`), or exit at once.
        fn set_mode(&self, mode: Option<&str>) {
            match mode {
                Some(mode) => std::fs::write(self.path("mode"), mode).unwrap(),
                None => std::fs::remove_file(self.path("mode")).unwrap(),
            }
        }

        /// Waits until `n` windows have recorded their run (and, kept running, are ready
        /// for signals).
        async fn wait_runs(&self, n: usize) {
            timeout(LIMIT, async {
                while self.runs().len() < n {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{} of {n} windows ran", self.runs().len()));
        }
    }

    /// Waits for the last window `launcher` started to exit and be reaped.
    async fn settled(launcher: &WindowLauncher) {
        let mut ended = launcher
            .own
            .lock()
            .await
            .as_ref()
            .expect("a window was started")
            .ended
            .clone();
        timeout(LIMIT, ended.wait_for(|ended| *ended))
            .await
            .expect("the window was never reaped")
            .unwrap();
    }

    /// Whether `pid` is a live process (or one not yet reaped).
    fn alive(pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    #[tokio::test]
    async fn launcher_reaps_child() {
        let windows = Arc::new(FakeWindows::default());
        let launcher =
            WindowLauncher::new(windows.clone(), PathBuf::from("/bin/true"), Launch::Direct);
        assert_eq!(launcher.last_exit(), None);
        launcher.open_or_raise(None).await.unwrap();
        let status = timeout(Duration::from_secs(1), async {
            loop {
                if let Some(status) = launcher.last_exit() {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the window was never waited on");
        assert!(status.success(), "{status:?}");
        // No window was connected, so none was closed.
        assert!(windows.closes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn launcher_closes_open_window_first() {
        let fake = FakeWindow::new();
        let windows = Arc::new(FakeWindows {
            connected: AtomicBool::new(true),
            ran: Some(fake.path("ran")),
            ..FakeWindows::default()
        });
        let launcher = WindowLauncher::new(windows.clone(), fake.link.clone(), Launch::Direct);
        launcher.open_or_raise(None).await.unwrap();
        assert_eq!(*windows.closes.lock().unwrap(), [CLOSE_WAIT]);
        // The fresh window hadn't run when the old one was asked to close …
        assert_eq!(*windows.ran_at_close.lock().unwrap(), [false]);
        // … and ran after.
        settled(&launcher).await;
        assert!(launcher.last_exit().unwrap().success());
        assert_eq!(fake.runs(), ["<none>"]);
    }

    #[tokio::test]
    async fn launcher_passes_the_activation_token() {
        let fake = FakeWindow::new();
        let launcher = WindowLauncher::new(
            Arc::new(FakeWindows::default()),
            fake.link.clone(),
            Launch::Direct,
        );
        launcher
            .open_or_raise(Some("token-1".into()))
            .await
            .unwrap();
        settled(&launcher).await;
        // None, even if the engine was itself started with one: that one is spent.
        launcher.open_or_raise(None).await.unwrap();
        settled(&launcher).await;
        assert_eq!(fake.runs(), ["token-1", "<none>"]);
    }

    #[tokio::test]
    async fn launcher_opens_one_window_at_a_time() {
        let fake = FakeWindow::new();
        let hold = Arc::new(Notify::new());
        let windows = Arc::new(FakeWindows {
            connected: AtomicBool::new(true),
            hold: Some(hold.clone()),
            ..FakeWindows::default()
        });
        let launcher = Arc::new(WindowLauncher::new(
            windows.clone(),
            fake.link.clone(),
            Launch::Direct,
        ));
        let first = tokio::spawn({
            let launcher = launcher.clone();
            async move { launcher.open_or_raise(None).await }
        });
        // The first is closing the open window …
        timeout(LIMIT, async {
            while windows.held.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the first click never asked the window to close");
        // … so a second click does nothing, at once.
        let second = timeout(Duration::from_secs(1), launcher.open_or_raise(None))
            .await
            .expect("a second click waited for the first");
        assert!(matches!(second, Err(OpenError::Busy)), "{second:?}");
        hold.notify_one();
        first.await.unwrap().unwrap();
        settled(&launcher).await;
        assert_eq!(windows.closes.lock().unwrap().len(), 1);
        assert_eq!(fake.runs().len(), 1);
        // Once it is done, the next click opens again.
        launcher.open_or_raise(None).await.unwrap();
        settled(&launcher).await;
        assert_eq!(fake.runs().len(), 2);
    }

    #[tokio::test]
    async fn launcher_stops_its_own_window_that_wont_close() {
        let fake = FakeWindow::new();
        fake.set_mode(Some("stay"));
        let windows = Arc::new(FakeWindows::default());
        let launcher = WindowLauncher::new(windows.clone(), fake.link.clone(), Launch::Direct);
        launcher.open_or_raise(None).await.unwrap();
        fake.wait_runs(1).await;
        let old = fake.pids()[0];
        // It connected, and now ignores being asked to close.
        windows.connected.store(true, Ordering::SeqCst);
        windows.stays_open.store(true, Ordering::SeqCst);
        fake.set_mode(None);
        launcher.open_or_raise(None).await.unwrap();
        assert_eq!(*windows.closes.lock().unwrap(), [CLOSE_WAIT]);
        // SIGTERM ended it before the fresh one started.
        assert_eq!(fake.signals(), ["TERM"]);
        assert!(!alive(old), "the old window still runs");
        settled(&launcher).await;
        assert_eq!(fake.runs().len(), 2);
        assert!(launcher.last_exit().unwrap().success());
    }

    #[tokio::test]
    async fn launcher_kills_its_own_window_that_ignores_sigterm() {
        let fake = FakeWindow::new();
        fake.set_mode(Some("stubborn"));
        let windows = Arc::new(FakeWindows::default());
        let launcher = WindowLauncher::new(windows.clone(), fake.link.clone(), Launch::Direct);
        launcher.open_or_raise(None).await.unwrap();
        fake.wait_runs(1).await;
        let old = fake.pids()[0];
        windows.connected.store(true, Ordering::SeqCst);
        windows.stays_open.store(true, Ordering::SeqCst);
        fake.set_mode(None);
        let started = Instant::now();
        launcher.open_or_raise(None).await.unwrap();
        // SIGTERM was ignored for `STOP_GRACE`; SIGKILL ended it.
        assert!(started.elapsed() >= STOP_GRACE, "{:?}", started.elapsed());
        assert!(!alive(old), "the old window still runs");
        settled(&launcher).await;
        assert_eq!(fake.runs().len(), 2);
    }

    #[tokio::test]
    async fn launcher_leaves_a_starting_window_to_come_up() {
        let fake = FakeWindow::new();
        fake.set_mode(Some("stay"));
        let windows = Arc::new(FakeWindows::default());
        let mut launcher = WindowLauncher::new(windows.clone(), fake.link.clone(), Launch::Direct);
        launcher.open_or_raise(None).await.unwrap();
        fake.wait_runs(1).await;
        // A double click: the window isn't connected yet, so the second does nothing.
        assert!(matches!(
            launcher.open_or_raise(None).await,
            Err(OpenError::Starting)
        ));
        assert_eq!(fake.runs().len(), 1);
        assert!(windows.closes.lock().unwrap().is_empty());
        // Past the grace, a window that never connected is stuck: stopped, and replaced.
        launcher.start_grace = Duration::ZERO;
        fake.set_mode(None);
        launcher.open_or_raise(None).await.unwrap();
        assert_eq!(fake.signals(), ["TERM"]);
        settled(&launcher).await;
        assert_eq!(fake.runs().len(), 2);
    }

    #[tokio::test]
    async fn launcher_opens_beside_a_window_it_did_not_start() {
        let fake = FakeWindow::new();
        let windows = Arc::new(FakeWindows::stuck());
        let launcher = WindowLauncher::new(windows.clone(), fake.link.clone(), Launch::Direct);
        launcher.open_or_raise(None).await.unwrap();
        assert_eq!(*windows.closes.lock().unwrap(), [CLOSE_WAIT]);
        settled(&launcher).await;
        assert_eq!(fake.runs().len(), 1);
    }

    #[tokio::test]
    async fn launcher_reports_a_missing_window_binary() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join(WINDOW_BINARY);
        let launcher = WindowLauncher::new(
            Arc::new(FakeWindows::default()),
            missing.clone(),
            // Not even a scope is started for a window that isn't there.
            Launch::Scope {
                systemd_run: PathBuf::from("/nonexistent/systemd-run"),
            },
        );
        match launcher.open_or_raise(None).await {
            Err(e @ OpenError::Spawn { .. }) => {
                assert_eq!(
                    e.to_string(),
                    format!(
                        "couldn't start the window ({}): not found beside the engine or on $PATH",
                        missing.display()
                    )
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(launcher.last_exit(), None);
        // A failed start doesn't leave the launcher busy.
        assert!(matches!(
            launcher.open_or_raise(None).await,
            Err(OpenError::Spawn { .. })
        ));
    }

    #[test]
    fn window_command_runs_the_window_in_its_own_scope() {
        let binary = Path::new("/usr/bin/loudauto");
        assert_eq!(
            window_command(&Launch::Direct, binary, "18f3a"),
            (binary.to_path_buf(), vec![])
        );
        let scope = Launch::Scope {
            systemd_run: PathBuf::from("/usr/bin/systemd-run"),
        };
        let (program, args) = window_command(&scope, binary, "18f3a");
        assert_eq!(program, Path::new("/usr/bin/systemd-run"));
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--unit=app-to.skunk.loudauto-18f3a.scope",
                "--",
                "/usr/bin/loudauto",
            ]
        );
    }

    #[test]
    fn find_on_path_takes_the_first_dir_that_has_it() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(b.path().join(SYSTEMD_RUN), "").unwrap();
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        let name = OsStr::new(SYSTEMD_RUN);
        assert_eq!(
            find_on_path(name, Some(&path)),
            Some(b.path().join(SYSTEMD_RUN))
        );
        std::fs::write(a.path().join(SYSTEMD_RUN), "").unwrap();
        assert_eq!(
            find_on_path(name, Some(&path)),
            Some(a.path().join(SYSTEMD_RUN))
        );
        // A dir isn't a program; no `$PATH` finds nothing.
        std::fs::create_dir(a.path().join("dir")).unwrap();
        assert_eq!(find_on_path(OsStr::new("dir"), Some(&path)), None);
        assert_eq!(find_on_path(name, None), None);
        assert_eq!(find_on_path(name, Some(OsStr::new(""))), None);
        // A path is taken as it is; a bare name is looked up.
        assert_eq!(
            resolve(&a.path().join(SYSTEMD_RUN), None),
            Some(a.path().join(SYSTEMD_RUN))
        );
        assert_eq!(resolve(&b.path().join("missing"), Some(&path)), None);
        assert_eq!(
            resolve(Path::new(SYSTEMD_RUN), Some(&path)),
            Some(a.path().join(SYSTEMD_RUN))
        );
    }

    #[test]
    fn window_binary_is_the_engines_sibling_else_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("loudautod");
        // No sibling: `loudauto`, for `$PATH`.
        assert_eq!(window_binary(Some(&exe)), PathBuf::from(WINDOW_BINARY));
        assert_eq!(window_binary(None), PathBuf::from(WINDOW_BINARY));
        std::fs::write(dir.path().join(WINDOW_BINARY), "").unwrap();
        assert_eq!(window_binary(Some(&exe)), dir.path().join(WINDOW_BINARY));
    }
}
