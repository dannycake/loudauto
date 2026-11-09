//! Starting and stopping the engine: bind the socket (refusing if another engine answers
//! on it), build the `Engine` and its loops, serve clients, MPRIS, the tray and the heart
//! key until `Request::Quit`, SIGTERM or SIGINT, then stop Soloist cleanly. Without a
//! tray host the engine runs on without its tray icon; without the shortcuts portal,
//! without its heart key.

use std::{
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use engine_ipc::{EngineClient, IpcError, Role};
use log::{info, warn};
use playback::config::SoloistConfig;
use spot_core::{
    creds::SecretServiceStore,
    library::Library,
    session::SessionCell,
    web::{
        SessionTokens, Web,
        lanes::{BACKGROUND_GAP, Lanes},
        registry::HashRegistry,
    },
};
use tokio::{
    net::UnixListener,
    signal::unix::{SignalKind, signal},
    sync::{broadcast, mpsc, watch},
};

use crate::{
    core::Core,
    desktop::{KwalletKey, LiveDesktop},
    engine::{Engine, Parts},
    hinting::Hinting,
    mpris::spawn_mpris,
    pages::{PageService, WebPageSource},
    server,
    session::run_session,
    settings::{self, Saver},
    shortcut::heart_key,
    sleep::watch_sleep,
    soloist::run_soloist,
    tray::run_tray,
    window::{self, Launch, WindowLauncher},
};

/// Events waiting for the slowest client; past this it is hung up on and reconnects.
const EVENTS_CAPACITY: usize = 1024;
/// How long quitting waits for Soloist to stop.
const SOLOIST_STOP_LIMIT: Duration = Duration::from_secs(15);

/// Why the socket couldn't be bound.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("another LOUDAUTO engine is already running (it answers on {0})")]
    Running(PathBuf),
    #[error("something holds {0} but doesn't answer as an engine; stop it first")]
    NotAnswering(PathBuf),
    #[error("engine socket {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

/// Binds the engine's socket at `path`: its directory 0700, the socket 0600. A socket
/// another engine answers on is left alone; one nothing answers on is stale, and
/// replaced.
pub async fn bind(path: &Path) -> Result<UnixListener, BindError> {
    let io = |source| BindError::Io {
        path: path.to_path_buf(),
        source,
    };
    engine_ipc::ensure_socket_dir(path).map_err(io)?;
    match EngineClient::connect_at(path, Role::Cli).await {
        Ok(_) | Err(IpcError::VersionMismatch { .. }) => {
            return Err(BindError::Running(path.to_path_buf()));
        }
        // No socket, or one nothing listens on: an engine that died left it.
        Err(IpcError::NotRunning) => match std::fs::remove_file(path) {
            Ok(()) => info!("removed a stale engine socket at {}", path.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        },
        Err(IpcError::Timeout | IpcError::Closed) => {
            return Err(BindError::NotAnswering(path.to_path_buf()));
        }
        Err(IpcError::Io(e)) => return Err(io(e)),
        Err(IpcError::Engine(e)) => return Err(io(io::Error::other(e))),
    }
    let listener = UnixListener::bind(path).map_err(io)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    Ok(listener)
}

/// Runs the engine on the socket at `socket` until it is told to quit. `Err` is why it
/// couldn't start.
pub async fn run(socket: PathBuf) -> Result<(), String> {
    // First, so a SIGTERM during the start still stops Soloist cleanly.
    let mut term = signal(SignalKind::terminate()).map_err(|e| format!("SIGTERM: {e}"))?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| format!("SIGINT: {e}"))?;
    let listener = bind(&socket).await.map_err(|e| e.to_string())?;
    info!("listening on {}", socket.display());

    let settings_path = settings::default_path();
    let settings = settings::load(&settings_path, settings::defaults()).await;
    let (events, _) = broadcast::channel(EVENTS_CAPACITY);

    // The session: `run_session` is the cell's only writer.
    let cell = Arc::new(SessionCell::empty());
    let lanes = Arc::new(Lanes::new(BACKGROUND_GAP));
    let library = Arc::new(Library::new(cell.clone(), lanes.clone()));
    let registry = Arc::new(HashRegistry::load(HashRegistry::default_path()).await);
    let web = Arc::new(Web::new(
        Arc::new(SessionTokens(cell.clone())),
        lanes,
        registry,
    ));
    let (hints, hints_rx) = mpsc::channel(16);
    let (updates, updates_rx) = mpsc::channel(16);
    tokio::spawn(run_session(
        cell,
        Arc::new(SecretServiceStore::SPOTIFY_SESSION),
        updates,
        hints_rx,
    ));
    let wake = watch_sleep(hints.clone());

    // Soloist, playing into the owner's device.
    let mut cfg = SoloistConfig::from_env();
    cfg.pipewire_device = settings.output_device.clone();
    let (to_core, to_core_rx) = mpsc::channel(256);
    let (health, health_rx) = watch::channel(engine_ipc::SoloistHealth::Starting);
    let (soloist, soloist_rx) = mpsc::channel(32);
    let (notices, notices_rx) = mpsc::channel(8);
    let soloist_loop = tokio::spawn(run_soloist(
        cfg,
        Arc::new(KwalletKey),
        Arc::new(Hinting::new(web.clone(), hints.clone())),
        to_core,
        health,
        soloist_rx,
        wake,
        notices,
    ));

    // Whether the session is ready: what the pages and the liked set wait for.
    let (ready, ready_rx) = watch::channel(false);
    let pages = PageService::new(
        Arc::new(Hinting::new(
            WebPageSource::new(web.clone(), library.clone()),
            hints.clone(),
        )),
        PageService::default_dir(),
        events.clone(),
    )
    .waiting_for_session(ready_rx.clone());
    let engine = Engine::new(Parts {
        core: Core::new(settings),
        events,
        pages,
        liker: Arc::new(Hinting::new(web.clone(), hints.clone())),
        web,
        library,
        soloist,
        hints,
        saver: Saver::new(settings_path),
        desktop: Arc::new(LiveDesktop),
    });

    tokio::spawn(engine.clone().on_playback(to_core_rx));
    tokio::spawn(engine.clone().on_session(updates_rx, ready));
    tokio::spawn(engine.clone().on_soloist_health(health_rx));
    tokio::spawn(engine.clone().on_notices(notices_rx));
    tokio::spawn(engine.clone().refresh_liked(ready_rx));
    tokio::spawn(engine.clone().refresh_hashes());
    let server = tokio::spawn(server::serve(listener, engine.clone()));
    // Media keys, Plasma's widget and the lock screen, on their own thread; the tray.
    let (local_requests, local_rx) = mpsc::channel(16);
    tokio::spawn(engine.clone().on_local_requests(local_rx));
    spawn_mpris(engine.player_state(), local_requests.clone());
    let launcher = WindowLauncher::new(
        engine.windows().clone(),
        window::default_window_binary(),
        Launch::detect(),
    );
    tokio::spawn(run_tray(
        engine.player_state(),
        local_requests.clone(),
        Arc::new(launcher),
    ));
    // The global heart key; without the portal, a notice says how to get one anyway.
    tokio::spawn(heart_key(engine.clone(), local_requests));

    let why = tokio::select! {
        () = engine.quit_requested() => "asked to quit",
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    };
    info!("quitting ({why}): stopping Soloist");
    server.abort();
    engine.close_soloist();
    match tokio::time::timeout(SOLOIST_STOP_LIMIT, soloist_loop).await {
        Ok(_) => info!("Soloist stopped"),
        Err(_) => warn!(
            "Soloist didn't stop in {} s; quitting anyway",
            SOLOIST_STOP_LIMIT.as_secs()
        ),
    }
    engine.flush_settings().await;
    if let Err(e) = std::fs::remove_file(&socket) {
        warn!("couldn't remove {}: {e}", socket.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::testing::{settings, test_engine};

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn binds_private_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loudauto/engine.sock");
        let _listener = bind(&path).await.unwrap();
        assert_eq!(mode(&dir.path().join("loudauto")), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[tokio::test]
    async fn replaces_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.sock");
        // A socket left by an engine that died: bound, then its listener gone.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let listener = bind(&path).await.unwrap();
        // It is ours now: a client gets through.
        let accept = tokio::spawn(async move { listener.accept().await.is_ok() });
        tokio::net::UnixStream::connect(&path).await.unwrap();
        assert!(accept.await.unwrap());
    }

    #[tokio::test]
    async fn refuses_while_another_engine_answers() {
        let t = test_engine(Core::new(settings())).await;
        let path = t.dir.path().join("engine.sock");
        let listener = bind(&path).await.unwrap();
        tokio::spawn(server::serve(listener, t.engine.clone()));
        match bind(&path).await {
            Err(BindError::Running(at)) => assert_eq!(at, path),
            other => panic!("{other:?}"),
        }
        // The running engine's socket is untouched.
        EngineClient::connect_at(&path, Role::Cli).await.unwrap();
        assert_eq!(
            BindError::Running(path.clone()).to_string(),
            format!(
                "another LOUDAUTO engine is already running (it answers on {})",
                path.display()
            )
        );
    }
}
