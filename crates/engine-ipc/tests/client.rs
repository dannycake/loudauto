//! `EngineClient` against a scripted engine on a socket in a temporary directory.

use std::{future::Future, path::PathBuf, time::Duration};

use engine_ipc::{
    ClientMsg, EngineClient, EngineState, Event, IpcError, Level, LoginStep, Notice,
    PROTOCOL_VERSION, PageRequest, PlayerState, Reply, Request, Role, ServerMsg, SessionHealth,
    Settings, SoloistHealth,
    frame::{read_frame, write_frame},
};
use playback::{
    command::Repeat,
    event::{Actions, PlayStatus},
};
use tempfile::TempDir;
use tokio::{
    net::{UnixListener, UnixStream},
    task::JoinHandle,
    time::Instant,
};

fn state() -> EngineState {
    EngineState {
        player: PlayerState {
            track: None,
            status: PlayStatus::Stopped,
            active_here: false,
            context_uri: None,
            position: None,
            volume: 40,
            shuffle: false,
            repeat: Repeat::Off,
            actions: Actions::default(),
            liked: None,
            quality: None,
        },
        queue: vec![],
        soloist: SoloistHealth::Ready,
        session: SessionHealth::Ready,
        login: LoginStep::LoggedIn,
        settings: Settings {
            volume: 40,
            output_device: None,
            autostart: false,
        },
        username: Some("someone".into()),
        soloist_expires_in_days: None,
    }
}

fn notice(id: u64) -> Event {
    Event::Notice(Notice {
        id,
        level: Level::Info,
        text: format!("notice {id}"),
    })
}

/// A listening socket in a fresh temporary directory.
fn listen() -> (TempDir, PathBuf, UnixListener) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.sock");
    let listener = UnixListener::bind(&path).unwrap();
    (dir, path, listener)
}

/// Accept one client, expect its Hello, answer Welcome with `version`, then run `script`.
fn engine<F, Fut>(listener: UnixListener, version: u32, script: F) -> JoinHandle<()>
where
    F: FnOnce(UnixStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello: ClientMsg = read_frame(&mut stream).await.unwrap().unwrap();
        assert_eq!(
            hello,
            ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                role: Role::Window
            }
        );
        let welcome = ServerMsg::Welcome {
            version,
            state: Box::new(state()),
        };
        write_frame(&mut stream, &welcome).await.unwrap();
        script(stream).await;
    })
}

/// The next request from the client.
async fn next_request(stream: &mut UnixStream) -> (u64, Request) {
    match read_frame(stream).await.unwrap() {
        Some(ClientMsg::Request { id, req }) => (id, req),
        other => panic!("expected a request, got {other:?}"),
    }
}

async fn reply(stream: &mut UnixStream, id: u64, result: Result<Reply, String>) {
    write_frame(stream, &ServerMsg::Reply { id, result })
        .await
        .unwrap();
}

#[tokio::test]
async fn client_request_reply() {
    let (_dir, path, listener) = listen();
    let server = engine(listener, PROTOCOL_VERSION, |mut s| async move {
        let (id, req) = next_request(&mut s).await;
        assert_eq!(req, Request::Play);
        reply(&mut s, id, Ok(Reply::Ok)).await;
        // Hold the connection until the client hangs up.
        let rest: Option<ClientMsg> = read_frame(&mut s).await.unwrap();
        assert_eq!(rest, None);
    });

    let (client, welcome, _events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
    assert_eq!(welcome, state());
    assert!(matches!(client.request(Request::Play).await, Ok(Reply::Ok)));
    drop(client);
    server.await.unwrap();
}

#[tokio::test]
async fn engine_error_reaches_the_caller() {
    let (_dir, path, listener) = listen();
    let _server = engine(listener, PROTOCOL_VERSION, |mut s| async move {
        let (id, _) = next_request(&mut s).await;
        reply(&mut s, id, Err("Couldn't like: 500".into())).await;
        let _ = read_frame::<_, ClientMsg>(&mut s).await;
    });

    let (client, _, _events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
    match client.request(Request::ToggleHeart { uri: None }).await {
        Err(IpcError::Engine(msg)) => assert_eq!(msg, "Couldn't like: 500"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn replies_and_events_are_demultiplexed() {
    let (_dir, path, listener) = listen();
    let _server = engine(listener, PROTOCOL_VERSION, |mut s| async move {
        let answer = |req: &Request| match req {
            Request::Pause => Reply::Dropped,
            _ => Reply::LikedState(vec![Some(true)]),
        };
        let (first, first_req) = next_request(&mut s).await;
        let (second, second_req) = next_request(&mut s).await;
        // Answer the later request first, with events in between.
        write_frame(&mut s, &ServerMsg::Event(notice(1)))
            .await
            .unwrap();
        reply(&mut s, second, Ok(answer(&second_req))).await;
        write_frame(&mut s, &ServerMsg::Event(notice(2)))
            .await
            .unwrap();
        reply(&mut s, first, Ok(answer(&first_req))).await;
        let _ = read_frame::<_, ClientMsg>(&mut s).await;
    });

    let (client, _, mut events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
    let (a, b) = tokio::join!(
        client.request(Request::Pause),
        client.request(Request::LikedState {
            uris: vec!["spotify:track:t".into()]
        }),
    );
    assert!(matches!(a, Ok(Reply::Dropped)), "{a:?}");
    assert!(
        matches!(b, Ok(Reply::LikedState(ref v)) if v == &[Some(true)]),
        "{b:?}"
    );
    assert_eq!(events.recv().await, Some(notice(1)));
    assert_eq!(events.recv().await, Some(notice(2)));
}

/// The window shares one client between tasks and spawns its requests.
#[allow(dead_code)]
fn client_and_requests_are_send(
    client: &EngineClient,
) -> impl Future<Output = Result<Reply, IpcError>> + Send + '_ {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<EngineClient>();
    client.request(Request::Play)
}

#[tokio::test]
async fn client_version_mismatch() {
    // A newer engine, and the version-1 engine this build replaced (its LikedState
    // answers were never unknown): both are a mismatch the owner is told about.
    for other in [PROTOCOL_VERSION + 1, 1] {
        let (_dir, path, listener) = listen();
        let _server = engine(listener, other, |_| async {});
        match EngineClient::connect_at(&path, Role::Window).await {
            Err(IpcError::VersionMismatch { engine, ours }) => {
                assert_eq!((engine, ours), (other, PROTOCOL_VERSION));
            }
            Err(e) => panic!("{e:?}"),
            Ok(_) => panic!("connected to a version-{other} engine"),
        }
    }
}

#[tokio::test]
async fn version_mismatch_found_even_when_state_is_unreadable() {
    // A future engine's Welcome may carry a state this build can't parse; the version
    // still has to be the error the user sees.
    let (_dir, path, listener) = listen();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let _: Option<ClientMsg> = read_frame(&mut s).await.unwrap();
        let future = PROTOCOL_VERSION + 1;
        let welcome =
            serde_json::json!({"t": "Welcome", "c": {"version": future, "state": {"new": 1}}});
        write_frame(&mut s, &welcome).await.unwrap();
        let _ = read_frame::<_, ClientMsg>(&mut s).await;
    });
    match EngineClient::connect_at(&path, Role::Window).await {
        Err(IpcError::VersionMismatch { engine, ours })
            if engine == PROTOCOL_VERSION + 1 && ours == PROTOCOL_VERSION => {}
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("connected"),
    }
}

#[tokio::test]
async fn client_timeout() {
    let (_dir, path, listener) = listen();
    let _server = engine(listener, PROTOCOL_VERSION, |mut s| async move {
        // Read everything, answer nothing, never hang up.
        while read_frame::<_, ClientMsg>(&mut s).await.unwrap().is_some() {}
    });
    let (client, _, _events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();

    tokio::time::pause();
    let start = Instant::now();
    let result = client.request(Request::Next).await;
    assert!(matches!(result, Err(IpcError::Timeout)), "{result:?}");
    assert_about(start.elapsed(), Duration::from_secs(10));
}

/// A page request waits 20 s, past the engine's own 15 s for an uncached page, so the
/// engine's answer (the page, or its timeout message) is never dropped.
#[tokio::test]
async fn page_request_waits_past_the_engine() {
    let (_dir, path, listener) = listen();
    let _server = engine(listener, PROTOCOL_VERSION, |mut s| async move {
        while read_frame::<_, ClientMsg>(&mut s).await.unwrap().is_some() {}
    });
    let (client, _, _events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();

    tokio::time::pause();
    let start = Instant::now();
    let page = Request::Page {
        req: PageRequest::Home,
        prefetch: false,
    };
    let result = client.request(page).await;
    assert!(matches!(result, Err(IpcError::Timeout)), "{result:?}");
    assert_about(start.elapsed(), Duration::from_secs(20));
}

#[tokio::test]
async fn events_end_on_disconnect() {
    let (_dir, path, listener) = listen();
    let _server = engine(listener, PROTOCOL_VERSION, |mut s| async move {
        write_frame(&mut s, &ServerMsg::Event(notice(1)))
            .await
            .unwrap();
        // The stream drops here: the engine went away.
    });
    let (client, _, mut events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
    assert_eq!(events.recv().await, Some(notice(1)));
    assert_eq!(events.recv().await, None);
    assert!(matches!(
        client.request(Request::Play).await,
        Err(IpcError::Closed)
    ));
}

#[tokio::test]
async fn pending_request_fails_when_engine_leaves() {
    let (_dir, path, listener) = listen();
    let _server = engine(listener, PROTOCOL_VERSION, |mut s| async move {
        let _ = next_request(&mut s).await;
        // Hang up without answering.
    });
    let (client, _, _events) = EngineClient::connect_at(&path, Role::Window).await.unwrap();
    let result = client.request(Request::Quit).await;
    assert!(matches!(result, Err(IpcError::Closed)), "{result:?}");
}

#[tokio::test]
async fn no_socket_is_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("engine.sock");
    let result = EngineClient::connect_at(&missing, Role::Cli).await;
    assert!(
        matches!(result, Err(IpcError::NotRunning)),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
async fn stale_socket_is_not_running() {
    // The engine died and left its socket file: connecting is refused.
    let (_dir, path, listener) = listen();
    drop(listener);
    assert!(path.exists());
    let result = EngineClient::connect_at(&path, Role::Cli).await;
    assert!(
        matches!(result, Err(IpcError::NotRunning)),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
async fn engine_that_never_welcomes_times_out() {
    let (_dir, path, listener) = listen();
    let _server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        while read_frame::<_, ClientMsg>(&mut s).await.unwrap().is_some() {}
    });
    tokio::time::pause();
    let start = Instant::now();
    let result = EngineClient::connect_at(&path, Role::Window).await;
    assert!(
        matches!(result, Err(IpcError::Timeout)),
        "{:?}",
        result.err()
    );
    assert_about(start.elapsed(), Duration::from_secs(5));
}

/// `elapsed` is `expected`, give or take the timer's millisecond rounding.
fn assert_about(elapsed: Duration, expected: Duration) {
    assert!(
        elapsed >= expected && elapsed < expected + Duration::from_millis(50),
        "{elapsed:?}, expected {expected:?}"
    );
}
