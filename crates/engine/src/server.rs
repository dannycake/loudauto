//! The engine's end of the socket. Each client opens with `Hello`; the engine answers
//! `Welcome` with the whole state, then forwards every broadcast event and answers each
//! request on its own task, so a slow request never holds up events or other requests.
//!
//! - A `Hello` of another protocol version still gets a `Welcome` carrying ours (the
//!   version is read from the raw JSON before anything else), then the engine hangs up:
//!   the client tells the owner which side is out of date.
//! - A client that falls too far behind the events is hung up on: it reconnects and
//!   starts again from a fresh `Welcome`, instead of drawing from a state with holes.
//! - The engine tracks the connected windows (`WindowControl`): notifications only show
//!   while none is open, and there is only ever one window: a window connecting closes
//!   the one already open (as the tray's raise does) before it is welcomed, whoever
//!   started it (the app launcher, KRunner, a second `loudauto`).

use std::{io, sync::Arc, time::Duration};

use async_trait::async_trait;
use engine_ipc::{
    PROTOCOL_VERSION,
    frame::{read_frame, write_frame},
    proto::{ClientMsg, Event, Role, ServerMsg},
};
use log::{debug, info, warn};
use serde_json::Value;
use tokio::{
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{
        broadcast::{self, error::RecvError},
        mpsc, watch,
    },
    time::timeout,
};

use crate::{engine::Engine, window::CLOSE_WAIT};

/// How long a new connection may take to say `Hello`.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// The answer to a request the engine can't read (another build's request).
pub const UNKNOWN_REQUEST: &str = "The engine doesn't understand this request.";
/// The answer when a reply is over the frame limit.
pub const TOO_LARGE: &str = "The answer was too large to send.";
/// Replies waiting to be written to one client.
const OUT_CAPACITY: usize = 64;

/// What Task 20's window launcher needs of the server, so its tests can fake it.
#[async_trait]
pub trait WindowControl: Send + Sync {
    /// Whether a `Role::Window` client is connected.
    fn window_connected(&self) -> bool;
    /// Sends `Event::CloseWindow` to every connected window and waits up to `wait` for
    /// them all to disconnect: `true` once none is connected (at once, if none was).
    async fn close_window(&self, wait: Duration) -> bool;
}

/// The connected windows.
pub struct Windows {
    /// How many are connected.
    count: watch::Sender<usize>,
    /// Bumped for each `close_window`; every window connection watches it.
    close: watch::Sender<u64>,
}

impl Default for Windows {
    fn default() -> Self {
        Windows {
            count: watch::channel(0).0,
            close: watch::channel(0).0,
        }
    }
}

impl Windows {
    /// Counts a window as connected until the guard drops.
    pub fn join(self: &Arc<Self>) -> WindowGuard {
        self.count.send_modify(|count| *count += 1);
        WindowGuard(self.clone())
    }

    /// Wakes on each `close_window` after this call.
    fn close_requests(&self) -> watch::Receiver<u64> {
        self.close.subscribe()
    }
}

/// A connected window.
pub struct WindowGuard(Arc<Windows>);

impl Drop for WindowGuard {
    fn drop(&mut self) {
        self.0.count.send_modify(|count| *count -= 1);
    }
}

#[async_trait]
impl WindowControl for Windows {
    fn window_connected(&self) -> bool {
        *self.count.borrow() > 0
    }

    async fn close_window(&self, wait: Duration) -> bool {
        let mut count = self.count.subscribe();
        if *count.borrow_and_update() == 0 {
            return true;
        }
        self.close
            .send_modify(|asked| *asked = asked.wrapping_add(1));
        let closed = timeout(wait, async {
            // The sender lives in `self`, so this only ends with the count at zero.
            count.wait_for(|count| *count == 0).await.is_ok()
        })
        .await;
        matches!(closed, Ok(true))
    }
}

/// Serves clients on `listener` for as long as the engine runs.
pub async fn serve(listener: UnixListener, engine: Arc<Engine>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(connection(stream, engine.clone()));
            }
            // Out of file descriptors, say: wait instead of spinning.
            Err(e) => {
                warn!("engine socket: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// A client's `Hello`: its protocol version, and its role if it speaks ours.
#[derive(Debug, PartialEq)]
struct Hello {
    version: u64,
    role: Option<Role>,
}

/// The `Hello` in a client's first frame. The version is read from the raw JSON first,
/// so a client of another protocol (whose `Hello` may not parse) still learns ours.
fn hello_of(frame: &Value) -> Option<Hello> {
    if frame["t"] != "Hello" {
        return None;
    }
    let version = frame["version"].as_u64()?;
    let role = match serde_json::from_value(frame.clone()) {
        Ok(ClientMsg::Hello { role, .. }) => Some(role),
        _ => None,
    };
    Some(Hello { version, role })
}

/// One client, from its `Hello` until either side hangs up.
async fn connection(stream: UnixStream, engine: Arc<Engine>) {
    let (mut read, mut write) = stream.into_split();
    let hello = match timeout(HELLO_TIMEOUT, read_frame::<_, Value>(&mut read)).await {
        Ok(Ok(Some(frame))) => hello_of(&frame),
        Ok(Ok(None)) => return,
        Ok(Err(e)) => {
            debug!("client hung up before its Hello: {e}");
            return;
        }
        Err(_) => {
            debug!(
                "client said nothing in {} s; hung up",
                HELLO_TIMEOUT.as_secs()
            );
            return;
        }
    };
    let Some(hello) = hello else {
        warn!("a client's first message wasn't a Hello; hung up");
        return;
    };
    let ours = hello.version == u64::from(PROTOCOL_VERSION);
    let role = match hello.role {
        Some(role) if ours => role,
        None if ours => {
            warn!("a client's Hello didn't parse; hung up");
            return;
        }
        // Another protocol: tell it ours, whatever its role.
        _ => {
            info!(
                "a client speaking protocol {} connected; told it ours ({PROTOCOL_VERSION}) and hung up",
                hello.version
            );
            let (state, _) = engine.welcome().await;
            let welcome = ServerMsg::Welcome {
                version: PROTOCOL_VERSION,
                state: Box::new(state),
            };
            let _ = write_frame(&mut write, &welcome).await;
            return;
        }
    };

    // Only ever one window: the one open closes first (waiting up to CLOSE_WAIT; a window
    // that hasn't gone by then ends itself soon after).
    if role == Role::Window && !engine.windows().close_window(CLOSE_WAIT).await {
        warn!(
            "the open window didn't close within {} ms of another opening",
            CLOSE_WAIT.as_millis()
        );
    }
    // Before the Welcome, so a window counts as open as soon as it is told it is
    // connected; the close watch before the count, so no close falls between.
    let close = (role == Role::Window).then(|| engine.windows().close_requests());
    let _window = (role == Role::Window).then(|| engine.windows().join());
    let (state, events) = engine.welcome().await;
    let welcome = ServerMsg::Welcome {
        version: PROTOCOL_VERSION,
        state: Box::new(state),
    };
    if let Err(e) = write_frame(&mut write, &welcome).await {
        debug!("{role:?} client hung up before its Welcome: {e}");
        return;
    }
    debug!("{role:?} client connected");
    let (out, out_rx) = mpsc::channel(OUT_CAPACITY);
    tokio::select! {
        () = read_requests(read, role, engine.clone(), out) => {}
        () = write_out(write, out_rx, events, close) => {}
    }
    debug!("{role:?} client disconnected");
}

/// Reads a client's requests until it hangs up; each is answered on its own task.
async fn read_requests(
    mut read: OwnedReadHalf,
    role: Role,
    engine: Arc<Engine>,
    out: mpsc::Sender<ServerMsg>,
) {
    loop {
        let frame = match read_frame::<_, Value>(&mut read).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(e) => {
                // The frame wasn't read whole (or wasn't JSON): the stream can't be trusted.
                warn!("{role:?} client: {e}; hung up");
                return;
            }
        };
        match serde_json::from_value::<ClientMsg>(frame.clone()) {
            Ok(ClientMsg::Request { id, req }) => {
                debug!("{role:?} request {id}: {req:?}");
                let (engine, out) = (engine.clone(), out.clone());
                tokio::spawn(async move {
                    let result = engine.handle(role, req).await;
                    // The client may have gone meanwhile.
                    let _ = out.send(ServerMsg::Reply { id, result }).await;
                });
            }
            Ok(ClientMsg::Hello { .. }) => warn!("{role:?} client: a second Hello, ignored"),
            Err(e) => {
                warn!("{role:?} client: unreadable message ({e})");
                if let Some(id) = frame["id"].as_u64() {
                    let reply = ServerMsg::Reply {
                        id,
                        result: Err(UNKNOWN_REQUEST.into()),
                    };
                    if out.send(reply).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Writes a client's replies, the broadcast events and, for a window, `CloseWindow`.
async fn write_out(
    mut write: OwnedWriteHalf,
    mut out: mpsc::Receiver<ServerMsg>,
    mut events: broadcast::Receiver<Event>,
    mut close: Option<watch::Receiver<u64>>,
) {
    loop {
        let msg = tokio::select! {
            msg = out.recv() => match msg {
                Some(msg) => msg,
                None => return,
            },
            event = events.recv() => match event {
                Ok(event) => ServerMsg::Event(event),
                Err(RecvError::Lagged(missed)) => {
                    warn!("a client fell {missed} events behind; hung up so it starts afresh");
                    return;
                }
                Err(RecvError::Closed) => return,
            },
            () = close_asked(&mut close) => ServerMsg::Event(Event::CloseWindow),
        };
        if let Err(e) = send(&mut write, &msg).await {
            debug!("client connection: writing failed: {e}");
            return;
        }
    }
}

/// Returns at the next `close_window`; never, for a client that isn't a window.
async fn close_asked(close: &mut Option<watch::Receiver<u64>>) {
    if let Some(asked) = close {
        if asked.changed().await.is_ok() {
            return;
        }
        // The engine is going: no close will come.
        *close = None;
    }
    std::future::pending().await
}

/// Writes `msg`. A reply over the frame limit is answered with an error instead; an
/// event over it is skipped.
async fn send(write: &mut OwnedWriteHalf, msg: &ServerMsg) -> io::Result<()> {
    match write_frame(write, msg).await {
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => match msg {
            ServerMsg::Reply { id, .. } => {
                warn!("reply {id}: {e}; answered with an error");
                let reply = ServerMsg::Reply {
                    id: *id,
                    result: Err(TOO_LARGE.into()),
                };
                write_frame(write, &reply).await
            }
            _ => {
                warn!("an event wasn't sent: {e}");
                Ok(())
            }
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use engine_ipc::{EngineClient, IpcError, proto::Request};
    use playback::event::{NowPlaying, PlayStatus, PlaybackEvent};

    use super::*;
    use crate::{
        core::Core,
        engine::testing::{TestEngine, settings, test_engine},
    };

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

    /// An engine on fakes, served on a socket in its temp dir.
    async fn served(core: Core) -> (TestEngine, std::path::PathBuf) {
        let t = test_engine(core).await;
        let path = t.dir.path().join("engine.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(serve(listener, t.engine.clone()));
        (t, path)
    }

    #[tokio::test]
    async fn server_welcome_then_events() {
        let mut core = Core::new(settings());
        core.on_playback(&PlaybackEvent::Track(nerve()));
        core.on_playback(&PlaybackEvent::Volume(33));
        let expected = core.state.clone();
        let (t, path) = served(core).await;

        let (_client, state, mut events) =
            EngineClient::connect_at(&path, Role::Cli).await.unwrap();
        assert_eq!(state, expected);

        let mut player = expected.player.clone();
        player.status = PlayStatus::Playing;
        t.engine.broadcast(Event::Player(player.clone()));
        let got = timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap();
        assert_eq!(got, Some(Event::Player(player)));
    }

    #[tokio::test]
    async fn server_rejects_old_protocol() {
        let (_t, path) = served(Core::new(settings())).await;
        let stream = UnixStream::connect(&path).await.unwrap();
        let (mut read, mut write) = stream.into_split();
        // A Hello from protocol 0, in whatever shape it had: only `version` is read.
        let hello = serde_json::json!({"t": "Hello", "version": 0, "role": {"t": "Ancient"}});
        write_frame(&mut write, &hello).await.unwrap();
        let welcome: Value = read_frame(&mut read).await.unwrap().unwrap();
        assert_eq!(welcome["t"], "Welcome");
        assert_eq!(welcome["c"]["version"], u64::from(PROTOCOL_VERSION));
        // Then the engine hangs up.
        let after = timeout(Duration::from_secs(5), read_frame::<_, Value>(&mut read))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after, None);

        // Our own client reports the mismatch the other way round.
        let (_t, path) = served(Core::new(settings())).await;
        let stream = UnixStream::connect(&path).await.unwrap();
        let (mut read, mut write) = stream.into_split();
        let hello = serde_json::json!({"t": "Hello", "version": 99, "role": {"t": "Cli"}});
        write_frame(&mut write, &hello).await.unwrap();
        let welcome: Value = read_frame(&mut read).await.unwrap().unwrap();
        assert_eq!(welcome["c"]["version"], u64::from(PROTOCOL_VERSION));
    }

    #[tokio::test]
    async fn window_close_waits_for_disconnect() {
        let (t, path) = served(Core::new(settings())).await;
        let windows = t.engine.windows().clone();
        assert!(!windows.window_connected());
        // No window: nothing to wait for.
        assert!(windows.close_window(Duration::from_millis(1500)).await);

        let (window, _, mut events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
        // A CLI client is no window.
        let (_cli, _, mut cli_events) = EngineClient::connect_at(&path, Role::Cli).await.unwrap();
        assert!(windows.window_connected());
        // The window closes when told to.
        let closer = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if event == Event::CloseWindow {
                    drop(window);
                    return true;
                }
            }
            false
        });
        assert!(windows.close_window(Duration::from_millis(1500)).await);
        assert!(closer.await.unwrap(), "the window never got CloseWindow");
        assert!(!windows.window_connected());
        // The CLI client was not told to close.
        t.engine.broadcast(Event::Expiry { days: 9 });
        let next = timeout(Duration::from_secs(5), cli_events.recv())
            .await
            .unwrap();
        assert_eq!(next, Some(Event::Expiry { days: 9 }));
    }

    /// Final review I3: there is only ever one window. A second `loudauto` (the app
    /// launcher, KRunner) closes the one already open, as the tray's raise does, before
    /// it is welcomed; a CLI client closes nothing.
    #[tokio::test]
    async fn a_second_window_closes_the_first() {
        let (t, path) = served(Core::new(settings())).await;
        let (first, _, mut events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
        let closer = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if event == Event::CloseWindow {
                    drop(first);
                    return true;
                }
            }
            false
        });
        let (_cli, _, _cli_events) = EngineClient::connect_at(&path, Role::Cli).await.unwrap();
        let (_second, _, _) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
        let closed = timeout(Duration::from_secs(3), closer)
            .await
            .expect("the first window was never told to close")
            .unwrap();
        assert!(closed);
        assert!(t.engine.windows().window_connected(), "the second is open");
    }

    #[tokio::test]
    async fn window_that_stays_open_times_close_out() {
        let (t, path) = served(Core::new(settings())).await;
        let (_window, _, _events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
        let windows = t.engine.windows().clone();
        assert!(!windows.close_window(Duration::from_millis(200)).await);
        assert!(windows.window_connected());
    }

    #[tokio::test]
    async fn quit_closes_the_window_before_the_engine_quits() {
        let (t, path) = served(Core::new(settings())).await;
        let (window, _, mut events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
        // The window notes whether the engine had already quit when it was told to close.
        let closer = tokio::spawn({
            let engine = t.engine.clone();
            async move {
                while let Some(event) = events.recv().await {
                    if event == Event::CloseWindow {
                        // `quit_requested` is ready at once if a quit was requested.
                        let quit = timeout(Duration::ZERO, engine.quit_requested()).await;
                        drop(window);
                        return Some(quit.is_ok());
                    }
                }
                None
            }
        });
        let reply = t.engine.handle(Role::Cli, Request::Quit).await;
        assert_eq!(reply, Ok(engine_ipc::Reply::Ok));
        let closed = timeout(Duration::from_secs(5), closer)
            .await
            .expect("the window was never told to close")
            .unwrap();
        assert_eq!(closed, Some(false), "quit before the close");
        timeout(Duration::from_secs(5), t.engine.quit_requested())
            .await
            .expect("no quit after the close");
    }

    #[tokio::test]
    async fn requests_are_answered_concurrently() {
        let (mut t, path) = served(Core::new(settings())).await;
        let (client, _, _events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
        let client = Arc::new(client);
        // Play waits on Soloist, which the test holds; LikedState doesn't wait for it.
        let play = tokio::spawn({
            let client = client.clone();
            async move { client.request(Request::Play).await }
        });
        let (cmd, reply) = t.soloist.recv().await.unwrap();
        assert_eq!(cmd, crate::soloist::SoloistCmd::Play(None));
        let liked = client
            .request(Request::LikedState {
                uris: vec![nerve().uri],
            })
            .await
            .unwrap();
        assert_eq!(liked, engine_ipc::Reply::LikedState(vec![None]));
        reply.send(Err("The player is restarting.".into())).unwrap();
        match play.await.unwrap() {
            Err(IpcError::Engine(message)) => assert_eq!(message, "The player is restarting."),
            other => panic!("{other:?}"),
        }
    }

    /// Final review C1, I1 and I4, the engine's half of "the session gets ready after
    /// the window's Welcome" (the window started the engine; the engine restarted under
    /// an open window): the window is told the name and the session once they're ready;
    /// its hearts are unknown until Liked Songs has been read, then `LikedSetReady` and
    /// right; and its heart for a liked track, drawn not liked meanwhile, likes it and
    /// never unlikes it.
    #[tokio::test]
    async fn a_window_connected_before_the_session_hears_it_get_ready() {
        use engine_ipc::{LoginStep, Reply, SessionHealth};

        use crate::{
            engine::testing::{FakeLikedSongs, FakeLiker, test_engine_full},
            session::SessionUpdate,
        };

        let (t_uri, u_uri) = ("spotify:track:t".to_string(), "spotify:track:u".to_string());
        // t is liked on the account; the engine has yet to find out.
        let songs = FakeLikedSongs::answering(vec![Ok(vec![t_uri.clone()])]);
        let liker = FakeLiker {
            on_account: vec![t_uri.clone()],
            ..FakeLiker::default()
        };
        let t = test_engine_full(Core::new(settings()), liker, songs).await;
        let path = t.dir.path().join("engine.sock");
        tokio::spawn(serve(UnixListener::bind(&path).unwrap(), t.engine.clone()));
        let (window, state, mut events) =
            EngineClient::connect_at(&path, Role::Window).await.unwrap();
        assert_eq!(state.username, None);
        assert_eq!(state.session, SessionHealth::Connecting);
        let ask = || Request::LikedState {
            uris: vec![t_uri.clone(), u_uri.clone()],
        };
        let unknown = window.request(ask()).await.unwrap();
        assert_eq!(unknown, Reply::LikedState(vec![None, None]));
        // Its heart, drawn as not liked, is clicked meanwhile: a like.
        let set = Request::SetLiked {
            uri: t_uri.clone(),
            liked: true,
        };
        let liked = window.request(set).await.unwrap();
        assert_eq!(
            liked,
            Reply::Liked {
                uri: t_uri.clone(),
                liked: true
            }
        );

        // The session gets ready, in the supervisor's order, and Liked Songs is read.
        let (updates, updates_rx) = mpsc::channel(8);
        let (ready_tx, ready) = watch::channel(false);
        tokio::spawn(t.engine.clone().on_session(updates_rx, ready_tx));
        tokio::spawn(t.engine.clone().refresh_liked(ready));
        for update in [
            SessionUpdate::Login(LoginStep::LoggedIn),
            SessionUpdate::Username("dannycake".into()),
            SessionUpdate::Health(SessionHealth::Ready),
        ] {
            updates.send(update).await.unwrap();
        }
        let mut heard = Vec::new();
        while heard.last() != Some(&Event::LikedSetReady) {
            let event = timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap();
            heard.push(event.expect("the engine stayed"));
        }
        let at = |want: &Event| heard.iter().position(|e| e == want);
        let name = at(&Event::Username("dannycake".into())).expect("the name");
        let ready = at(&Event::Session(SessionHealth::Ready)).expect("the session");
        assert!(name < ready, "{heard:?}");
        assert_eq!(
            window.request(ask()).await.unwrap(),
            Reply::LikedState(vec![Some(true), Some(false)])
        );
        assert_eq!(
            *t.liker.calls.lock().unwrap(),
            [(t_uri, true)],
            "a like, never an unlike"
        );
    }

    #[tokio::test]
    async fn unknown_request_is_answered_and_the_connection_kept() {
        let (_t, path) = served(Core::new(settings())).await;
        let stream = UnixStream::connect(&path).await.unwrap();
        let (mut read, mut write) = stream.into_split();
        let hello = ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            role: Role::Cli,
        };
        write_frame(&mut write, &hello).await.unwrap();
        let _welcome: Value = read_frame(&mut read).await.unwrap().unwrap();
        let odd = serde_json::json!({"t": "Request", "id": 7, "req": {"t": "Teleport"}});
        write_frame(&mut write, &odd).await.unwrap();
        let reply: ServerMsg = read_frame(&mut read).await.unwrap().unwrap();
        assert_eq!(
            reply,
            ServerMsg::Reply {
                id: 7,
                result: Err(UNKNOWN_REQUEST.into())
            }
        );
        let ok = ClientMsg::Request {
            id: 8,
            req: Request::LikedState { uris: vec![] },
        };
        write_frame(&mut write, &ok).await.unwrap();
        let reply: ServerMsg = read_frame(&mut read).await.unwrap().unwrap();
        assert_eq!(
            reply,
            ServerMsg::Reply {
                id: 8,
                result: Ok(engine_ipc::Reply::LikedState(vec![]))
            }
        );
    }

    #[tokio::test]
    async fn silent_client_is_dropped() {
        tokio::time::pause();
        let (_t, path) = served(Core::new(settings())).await;
        let stream = UnixStream::connect(&path).await.unwrap();
        let (mut read, _write) = stream.into_split();
        // Nothing said: after HELLO_TIMEOUT the engine hangs up without a Welcome.
        let got = read_frame::<_, Value>(&mut read).await.unwrap();
        assert_eq!(got, None);
    }
}
