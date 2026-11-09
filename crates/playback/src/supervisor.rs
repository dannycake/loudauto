//! Runs the Soloist binary: prepares its data dir, starts it, waits for its WebSocket
//! port, logs its output, reports how it ended, and stops it.

use std::{
    collections::VecDeque,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use tokio::{
    fs::OpenOptions,
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{broadcast, mpsc, watch},
    task::JoinHandle,
    time::{Instant, sleep, timeout, timeout_at},
};

use crate::{
    Result, SoloistError,
    config::SoloistConfig,
    event::PlaybackEvent,
    prefs::ensure_quality_prefs_async,
    quality::{QualityParser, parse_expiry},
};

/// Lines of Soloist output kept for error messages.
const TAIL_LINES: usize = 20;
const POLL: Duration = Duration::from_millis(50);
/// How long a stopped Soloist gets before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long the pumps get, once Soloist has ended, to copy what is left in its pipes.
const DRAIN: Duration = Duration::from_secs(1);

type Tail = Arc<Mutex<VecDeque<String>>>;
type Log = Arc<tokio::sync::Mutex<tokio::fs::File>>;

/// How long Soloist gets to open its WebSocket.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);

/// How a Soloist process ended: what `Exited` carried.
#[derive(Debug, Clone)]
struct Ended {
    code: Option<i32>,
    tail: String,
}

/// A running Soloist. A task owns the process: it sends `PlaybackEvent::Exited` once when
/// the process ends, whatever ended it, and nothing after it (output still coming
/// [`DRAIN`] after the end is cut off). Dropping the `Supervisor` kills Soloist.
pub struct Supervisor {
    pid: u32,
    port: u16,
    data_dir: PathBuf,
    /// Signals for that task to deliver; when this sender is dropped it kills Soloist.
    signals: mpsc::UnboundedSender<Signal>,
    /// Set once Soloist has ended, its output is drained and `Exited` is sent.
    end: watch::Receiver<Option<Ended>>,
}

impl Supervisor {
    pub async fn start(
        config: &SoloistConfig,
        api_key: &str,
        events: broadcast::Sender<PlaybackEvent>,
    ) -> Result<Supervisor> {
        Self::start_with_timeout(config, api_key, events, START_TIMEOUT).await
    }

    pub async fn start_with_timeout(
        config: &SoloistConfig,
        api_key: &str,
        events: broadcast::Sender<PlaybackEvent>,
        timeout: Duration,
    ) -> Result<Supervisor> {
        if !config.binary.is_file() {
            return Err(SoloistError::BinaryMissing(config.binary.clone()));
        }
        // Before touching ws.port, the log or prefs: they belong to the running one.
        if let Some(pid) = running_soloist(&config.data_dir) {
            return Err(SoloistError::AlreadyRunning(pid));
        }
        tokio::fs::create_dir_all(&config.data_dir).await?;
        tokio::fs::create_dir_all(&config.cache_dir).await?;
        // A crashed run can leave ws.port behind; never connect to that.
        let port_file = config.data_dir.join("ws.port");
        match tokio::fs::remove_file(&port_file).await {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        ensure_quality_prefs_async(&config.data_dir).await?;

        // Verbose logging grows ~200 KB/hour: one file per run, plus the run before it.
        let log_path = config.cache_dir.join("soloist.log");
        match tokio::fs::rename(&log_path, config.cache_dir.join("soloist.log.1")).await {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let log: Log = Arc::new(tokio::sync::Mutex::new(
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&log_path)
                .await?,
        ));
        let mut command = Command::new(&config.binary);
        command
            .args(config.args(api_key))
            .envs(config.env.iter().map(|(k, v)| (k, v)))
            // The key goes only as `-k`: an exported copy must not reach Soloist's environment.
            .env_remove("SOLOIST_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A player killed by a signal runs no destructors, so kill_on_drop alone would leave
        // Soloist playing with nothing controlling it. The kernel's parent-death signal fires
        // when the *thread* that spawned the child exits: call this from a long-lived runtime
        // thread, never from `spawn_blocking` (those threads retire when idle).
        // SAFETY: prctl is async-signal-safe and touches no memory shared with the parent.
        unsafe {
            command.pre_exec(|| {
                nix::sys::prctl::set_pdeathsig(Signal::SIGTERM).map_err(std::io::Error::from)
            });
        }
        let mut child = command.spawn()?;
        let pid = child.id().expect("a child that was just spawned has a pid");
        let tail: Tail = Arc::default();
        let pumps = [
            child
                .stdout
                .take()
                .map(|s| pump(s, log.clone(), tail.clone(), events.clone())),
            child
                .stderr
                .take()
                .map(|s| pump(s, log.clone(), tail.clone(), events.clone())),
        ]
        .into_iter()
        .flatten()
        .collect();
        let (signals, signal_rx) = mpsc::unbounded_channel();
        let (end_tx, end) = watch::channel(None);
        let output = Output { pumps, log, tail };
        tokio::spawn(own_child(child, signal_rx, output, events, end_tx));

        let deadline = Instant::now() + timeout;
        loop {
            let ended = end.borrow().clone();
            if let Some(Ended { code, tail }) = ended {
                return Err(exit_error(code, tail));
            }
            if let Ok(text) = tokio::fs::read_to_string(&port_file).await
                && let Ok(port) = text.trim().parse::<u16>()
            {
                return Ok(Supervisor {
                    pid,
                    port,
                    data_dir: config.data_dir.clone(),
                    signals,
                    end,
                });
            }
            if Instant::now() >= deadline {
                return Err(SoloistError::StartTimeout); // dropping `signals` kills it
            }
            sleep(POLL).await;
        }
    }

    pub fn ws_port(&self) -> u16 {
        self.port
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Wait for Soloist to end, however it ends: its exit code, `None` if a signal ended it.
    /// `Exited` has been sent by the time this returns.
    pub async fn wait(&mut self) -> Option<i32> {
        self.ended().await.and_then(|e| e.code)
    }

    /// SIGTERM, then SIGKILL after 5 s. Returns once Soloist has ended and the pumps have
    /// drained its output into the log; `Exited` is sent as for any other end.
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.signals.send(Signal::SIGTERM);
        if timeout(STOP_GRACE, self.ended()).await.is_err() {
            let _ = self.signals.send(Signal::SIGKILL);
            let _ = self.ended().await;
        }
        // Soloist removes ws.port on a clean shutdown; make sure after a forced one too.
        let _ = tokio::fs::remove_file(self.data_dir.join("ws.port")).await;
        Ok(())
    }

    /// `None` only if the owning task is gone without saying (its runtime shut down).
    async fn ended(&mut self) -> Option<Ended> {
        self.end
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|end| end.clone())
    }
}

/// What copies Soloist's output, and where it goes.
struct Output {
    pumps: Vec<JoinHandle<()>>,
    log: Log,
    tail: Tail,
}

/// Own the Soloist process until it ends: deliver `signals` (once their sender is dropped,
/// kill it), then let the pumps drain, send `Exited` once and publish the end.
async fn own_child(
    mut child: Child,
    mut signals: mpsc::UnboundedReceiver<Signal>,
    output: Output,
    events: broadcast::Sender<PlaybackEvent>,
    end: watch::Sender<Option<Ended>>,
) {
    let Output { pumps, log, tail } = output;
    let mut supervised = true;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            signal = signals.recv(), if supervised => {
                let signal = signal.unwrap_or_else(|| {
                    // The Supervisor is gone: nothing would control Soloist any more.
                    supervised = false;
                    Signal::SIGKILL
                });
                // `id()` is None once the child is reaped, so a reused pid is never hit.
                if let Some(pid) = child.id() {
                    let _ = kill(Pid::from_raw(pid as i32), signal);
                }
            }
        }
    };
    // Let the pipes drain so the log and the tail hold Soloist's last words. A pump still
    // copying at the deadline (a child of Soloist's holds the pipe open) is stopped there,
    // so nothing is emitted or logged after `Exited`.
    let deadline = Instant::now() + DRAIN;
    let mut cut = false;
    for mut pump in pumps {
        if timeout_at(deadline, &mut pump).await.is_err() {
            pump.abort();
            // The abort lands at the pump's next await; wait for it.
            let _ = pump.await;
            cut = true;
        }
    }
    if cut {
        log::debug!("Soloist's output was still open {DRAIN:?} after it ended; cut off");
        // A stopped pump didn't land its last lines itself.
        let _ = log.lock().await.flush().await;
    }
    let code = status.ok().and_then(|s| s.code());
    let tail = tail
        .lock()
        .map(|t| t.iter().map(String::as_str).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    log::debug!("Soloist exited ({code:?})");
    let _ = events.send(PlaybackEvent::Exited {
        code,
        tail: tail.clone(),
    });
    let _ = end.send(Some(Ended { code, tail }));
}

/// The pid of a live Soloist using `data_dir`, from the `soloist.pid` it writes. A pid is
/// only trusted if that process's own arguments name this data dir (`-D <data_dir>`), so a
/// stale file whose pid has been reused does not count.
pub fn running_soloist(data_dir: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(data_dir.join("soloist.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let want = data_dir.as_os_str().as_encoded_bytes();
    let mut args = cmdline.split(|b| *b == 0);
    while let Some(arg) = args.next() {
        if arg == b"-D" && args.next() == Some(want) {
            return Some(pid);
        }
    }
    None
}

/// Copy one output pipe into the log (bytes as they came), the error tail, and quality and
/// expiry events. It reads to the end of the pipe: stopping early would close Soloist's end.
fn pump(
    pipe: impl AsyncRead + Unpin + Send + 'static,
    log: Log,
    tail: Tail,
    events: broadcast::Sender<PlaybackEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut quality = QualityParser::default();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            // Bytes, not `lines()`: a line that isn't UTF-8 must not end the pump.
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let raw = buf.strip_suffix(b"\n").unwrap_or(&buf);
            {
                let mut file = log.lock().await;
                let _ = file.write_all(raw).await;
                let _ = file.write_all(b"\n").await;
            }
            let text = String::from_utf8_lossy(raw);
            let line = text.strip_suffix('\r').unwrap_or(&text);
            if let Ok(mut t) = tail.lock() {
                if t.len() == TAIL_LINES {
                    t.pop_front();
                }
                t.push_back(line.to_string());
            }
            if let Some(q) = quality.feed(line) {
                let _ = events.send(PlaybackEvent::Quality(q));
            }
            if let Some(days) = parse_expiry(line) {
                let _ = events.send(PlaybackEvent::ExpiresInDays(days));
            }
        }
        // tokio's File writes in the background; land the last line before `Exited`.
        let _ = log.lock().await.flush().await;
    })
}

/// Map a Soloist exit code: 10 means the build expired.
pub fn exit_error(code: Option<i32>, tail: String) -> SoloistError {
    match code {
        Some(10) => SoloistError::Expired,
        code => SoloistError::Exited { code, tail },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::StreamQuality;

    const KEY: &str = "SECRET-KEY-123";

    fn config(mode: &str, dir: &tempfile::TempDir) -> SoloistConfig {
        SoloistConfig {
            binary: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-soloist.sh"),
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            device_name: "LOUDAUTO".into(),
            pipewire_device: None,
            env: vec![("FAKE_MODE".into(), mode.into())],
        }
    }

    /// The first event `want` accepts, skipping the rest; panics after 3 s.
    async fn recv_until<T>(
        rx: &mut broadcast::Receiver<PlaybackEvent>,
        mut want: impl FnMut(PlaybackEvent) -> Option<T>,
    ) -> T {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if let Some(t) = want(event) {
                            return t;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(e) => panic!("{e}"),
                }
            }
        })
        .await
        .expect("the event did not arrive")
    }

    fn exited(event: PlaybackEvent) -> Option<(Option<i32>, String)> {
        match event {
            PlaybackEvent::Exited { code, tail } => Some((code, tail)),
            _ => None,
        }
    }

    /// Every `Exited` already sent.
    fn exits_so_far(rx: &mut broadcast::Receiver<PlaybackEvent>) -> Vec<(Option<i32>, String)> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(exited)
            .collect()
    }

    fn signal(pid: u32, signal: Signal) {
        kill(Pid::from_raw(pid as i32), signal).unwrap();
    }

    #[tokio::test]
    async fn starts_and_reports_port_and_quality() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("ok", &dir);
        let (tx, mut rx) = broadcast::channel(16);
        let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        assert_eq!(sup.ws_port(), 45678);
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ev,
            PlaybackEvent::Quality(StreamQuality {
                codec: "flac".into(),
                bitrate_bps: 1_400_000,
                sample_rate_hz: 44_100
            })
        );
        sup.stop().await.unwrap();
        assert!(!cfg.data_dir.join("ws.port").exists());
    }

    #[tokio::test]
    async fn each_run_starts_a_fresh_log_and_keeps_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("ok", &dir);
        std::fs::create_dir_all(&cfg.cache_dir).unwrap();
        std::fs::write(cfg.cache_dir.join("soloist.log"), "previous run\n").unwrap();
        std::fs::write(cfg.cache_dir.join("soloist.log.1"), "run before that\n").unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        // The quality event means the fake's output has been pumped into the log.
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        sup.stop().await.unwrap();
        let log = std::fs::read_to_string(cfg.cache_dir.join("soloist.log")).unwrap();
        let prev = std::fs::read_to_string(cfg.cache_dir.join("soloist.log.1")).unwrap();
        assert!(!log.contains("previous run"), "{log}");
        assert!(log.contains("codec: flac"), "{log}");
        assert_eq!(prev, "previous run\n");
    }

    #[tokio::test]
    async fn second_start_refuses_and_leaves_the_running_one_alone() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("ok", &dir);
        let (tx, mut rx) = broadcast::channel(16);
        let first = Supervisor::start(&cfg, KEY, tx.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let got = Supervisor::start(&cfg, KEY, tx).await;
        assert!(
            matches!(got, Err(SoloistError::AlreadyRunning(pid)) if pid == first.pid()),
            "{:?}",
            got.as_ref().map(|s| s.ws_port())
        );
        assert!(cfg.data_dir.join("ws.port").exists());
        let log = std::fs::read_to_string(cfg.cache_dir.join("soloist.log")).unwrap();
        assert!(log.contains("codec: flac"), "{log}");
        first.stop().await.unwrap();
    }

    #[tokio::test]
    async fn pid_file_of_another_process_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("ok", &dir);
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        // Alive, but not a Soloist on this data dir: a stale file whose pid was reused.
        std::fs::write(
            cfg.data_dir.join("soloist.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        assert_eq!(running_soloist(&cfg.data_dir), None);
        std::fs::write(cfg.data_dir.join("soloist.pid"), "not a pid").unwrap();
        assert_eq!(running_soloist(&cfg.data_dir), None);
        let (tx, _rx) = broadcast::channel(16);
        let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        assert_eq!(running_soloist(&cfg.data_dir), Some(sup.pid()));
        sup.stop().await.unwrap();
    }

    /// A killed (not Ctrl-C'd) player runs no cleanup at all; Soloist must still go.
    #[test]
    fn soloist_dies_with_the_thread_that_started_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("ok", &dir);
        let pid = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let pid = rt.block_on(async {
                let (tx, _rx) = broadcast::channel(16);
                let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
                let pid = running_soloist(&cfg.data_dir).unwrap();
                // No stop(), no kill_on_drop: only the parent-death signal is left.
                std::mem::forget(sup);
                pid
            });
            // Dropping the runtime would drop the task that owns the child, and
            // kill_on_drop with it.
            std::mem::forget(rt);
            pid
        })
        .join()
        .unwrap();
        let dead = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !dead() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            dead(),
            "Soloist (pid {pid}) outlived the thread that started it"
        );
    }

    #[tokio::test]
    async fn stale_port_file_removed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("slow", &dir);
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        std::fs::write(cfg.data_dir.join("ws.port"), "1").unwrap();
        let (tx, _rx) = broadcast::channel(16);
        let got = Supervisor::start_with_timeout(&cfg, KEY, tx, Duration::from_millis(300)).await;
        assert!(
            matches!(got, Err(SoloistError::StartTimeout)),
            "{:?}",
            got.as_ref().map(|s| s.ws_port())
        );
    }

    #[tokio::test]
    async fn expired_build() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = broadcast::channel(16);
        let got = tokio::time::timeout(
            Duration::from_secs(2),
            Supervisor::start(&config("expired", &dir), KEY, tx),
        )
        .await
        .unwrap();
        assert!(matches!(got, Err(SoloistError::Expired)));
    }

    #[tokio::test]
    async fn locked_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        match Supervisor::start(&config("locked", &dir), KEY, tx).await {
            Err(SoloistError::Exited {
                code: Some(1),
                tail,
            }) => {
                assert!(tail.contains("another Spotify Soloist process"), "{tail}")
            }
            other => panic!("{:?}", other.map(|s| s.ws_port())),
        }
        // A start that fails is still one process end.
        let ends = exits_so_far(&mut rx);
        assert_eq!(ends.len(), 1, "{ends:?}");
        assert_eq!(ends[0].0, Some(1));
    }

    #[tokio::test]
    async fn missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config("ok", &dir);
        cfg.binary = "/nonexistent/soloist".into();
        let (tx, _rx) = broadcast::channel(16);
        let err = Supervisor::start(&cfg, KEY, tx).await.err().unwrap();
        assert!(matches!(err, SoloistError::BinaryMissing(_)));
        assert!(err.to_string().contains("downloads-and-updates"));
    }

    #[tokio::test]
    async fn key_not_in_log() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("ok", &dir);
        let (tx, _rx) = broadcast::channel(16);
        let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        sup.stop().await.unwrap();
        let log = std::fs::read_to_string(cfg.cache_dir.join("soloist.log")).unwrap();
        assert!(log.contains("codec: flac"), "{log}");
        assert!(!log.contains(KEY));
    }

    #[tokio::test]
    async fn pump_survives_non_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("nonutf8", &dir);
        let (tx, mut rx) = broadcast::channel(16);
        let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        let days = recv_until(&mut rx, |event| match event {
            PlaybackEvent::ExpiresInDays(days) => Some(days),
            _ => None,
        })
        .await;
        assert_eq!(days, 89);
        sup.stop().await.unwrap();
        // The log keeps Soloist's bytes as they came, and the pump read on to the end.
        let log = std::fs::read(cfg.cache_dir.join("soloist.log")).unwrap();
        let text = String::from_utf8_lossy(&log);
        assert!(log.windows(3).any(|w| w == b"\xff\xfe\n"), "{text}");
        assert!(text.contains("shutting down"), "{text}");
    }

    #[tokio::test]
    async fn exit_event_carries_code_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("crash", &dir);
        let (tx, mut rx) = broadcast::channel(16);
        let mut sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        signal(running_soloist(&cfg.data_dir).unwrap(), Signal::SIGUSR1);
        let (code, tail) = recv_until(&mut rx, exited).await;
        assert_eq!(code, Some(3));
        assert!(tail.contains("boom"), "{tail}");
        assert_eq!(sup.wait().await, Some(3));
        // Stopping a Soloist that already ended is harmless and says nothing more.
        sup.stop().await.unwrap();
        assert_eq!(exits_so_far(&mut rx), vec![]);
    }

    #[tokio::test]
    async fn killed_soloist_sends_exited_without_code() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("ok", &dir);
        let (tx, mut rx) = broadcast::channel(16);
        let mut sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        signal(running_soloist(&cfg.data_dir).unwrap(), Signal::SIGKILL);
        assert_eq!(sup.wait().await, None);
        let (code, tail) = recv_until(&mut rx, exited).await;
        assert_eq!(code, None);
        assert!(tail.contains("codec: flac"), "{tail}");
        sup.stop().await.unwrap();
        // A killed Soloist leaves ws.port behind; stop() removes it.
        assert!(!cfg.data_dir.join("ws.port").exists());
        assert_eq!(exits_so_far(&mut rx), vec![]);
    }

    #[tokio::test]
    async fn stop_drains_output_and_sends_one_exited() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("ok", &dir);
        let (tx, mut rx) = broadcast::channel(16);
        let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        sup.stop().await.unwrap();
        // Soloist's last words are in the log and the tail once stop() returns.
        let log = std::fs::read_to_string(cfg.cache_dir.join("soloist.log")).unwrap();
        assert!(log.contains("shutting down"), "{log}");
        let ends = exits_so_far(&mut rx);
        assert_eq!(ends.len(), 1, "{ends:?}");
        assert_eq!(ends[0].0, Some(0));
        assert!(ends[0].1.contains("shutting down"), "{}", ends[0].1);
    }

    /// Deferred Task 10 minor: output a pump hasn't copied by the drain deadline (a child
    /// of Soloist's still holding its pipe) is cut off there: nothing is emitted after
    /// `Exited`, and the log doesn't grow once the end is published.
    #[tokio::test]
    async fn nothing_comes_after_exited_from_a_pipe_left_open() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("orphan", &dir);
        let (tx, mut rx) = broadcast::channel(256);
        let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        sup.stop().await.unwrap();
        let log = cfg.cache_dir.join("soloist.log");
        let logged = std::fs::metadata(&log).unwrap().len();
        // The child prints for 3 s after Soloist's end.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let events: Vec<PlaybackEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let end = events
            .iter()
            .position(|e| matches!(e, PlaybackEvent::Exited { .. }))
            .expect("an Exited");
        assert_eq!(&events[end + 1..], [], "emitted after Exited");
        assert_eq!(
            std::fs::metadata(&log).unwrap().len(),
            logged,
            "logged after the end"
        );
    }

    #[tokio::test]
    async fn dropped_supervisor_kills_soloist() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config("ok", &dir);
        let (tx, mut rx) = broadcast::channel(16);
        let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        let pid = running_soloist(&cfg.data_dir).unwrap();
        drop(sup);
        let (code, _) = recv_until(&mut rx, exited).await;
        assert_eq!(code, None, "killed, not stopped");
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[tokio::test]
    async fn api_key_not_in_env() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config("env", &dir);
        let env_file = dir.path().join("env.txt");
        // As if it were exported: set_var can't be used safely in a threaded test, and the
        // configured and inherited environments meet in the same `Command`.
        cfg.env.push(("SOLOIST_API_KEY".into(), KEY.into()));
        cfg.env
            .push(("FAKE_ENV_FILE".into(), env_file.display().to_string()));
        let (tx, _rx) = broadcast::channel(16);
        let sup = Supervisor::start(&cfg, KEY, tx).await.unwrap();
        // The fake lists variable names only, and no message below prints what it wrote:
        // this process's environment may hold the owner's credentials.
        let listed = std::fs::read_to_string(&env_file).unwrap();
        let names: Vec<&str> = listed.lines().collect();
        assert!(
            names.contains(&"FAKE_MODE"),
            "the fake did not list its environment"
        );
        assert!(
            !names.contains(&"SOLOIST_API_KEY"),
            "SOLOIST_API_KEY reached Soloist's environment"
        );
        assert!(
            !listed.contains(KEY),
            "the key reached Soloist's environment"
        );
        // The key still arrives, as `-k`.
        let pid = running_soloist(&cfg.data_dir).unwrap();
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        let k = args
            .iter()
            .position(|a| *a == b"-k")
            .expect("no -k in argv");
        assert!(
            args.get(k + 1) == Some(&KEY.as_bytes()),
            "-k does not carry the key"
        );
        sup.stop().await.unwrap();
    }

    #[test]
    fn exit_codes() {
        assert!(matches!(
            exit_error(Some(10), String::new()),
            SoloistError::Expired
        ));
        assert!(matches!(
            exit_error(Some(1), "t".into()),
            SoloistError::Exited { code: Some(1), .. }
        ));
        assert!(matches!(
            exit_error(None, "t".into()),
            SoloistError::Exited { code: None, .. }
        ));
    }
}
