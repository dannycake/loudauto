//! The window's link to the engine: a thread of its own, with a current-thread tokio
//! runtime, holds the connection. The UI thread only ever sends requests into it and
//! drains what came back (`poll`), so it never waits on the socket.
//!
//! The link reconnects by itself: when the engine goes (a restart under systemd, say),
//! `poll` yields `Disconnected`, and `Connected` with the engine's whole state once it is
//! back. A request made while there is no connection is answered at once with an error;
//! the window asks again after `Connected`. An engine that isn't running is started
//! (`systemctl --user start loudauto.service`) when the window opens, and once more after
//! each disconnect, if it is still down once systemd's own restart has had its chance.
//!
//! When the engine asks the window to close (another window is opening, or the engine is
//! quitting), the process ends [`CLOSE_BACKSTOP`] later even if egui never runs another
//! pass to act on it (a minimized window on KWin), so there is only ever one window.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    cell::Cell,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{self, ViewportCommand, ViewportId};
use engine_ipc::{EngineClient, EngineState, Event, IpcError, Reply, Request, Role};
use log::{debug, info, warn};
use tokio::{process::Command, sync::mpsc as async_mpsc, task::JoinSet};

/// A request's id, the link's own: `poll`'s `Reply` carries it.
pub type RequestId = u64;

/// The answer to a request made while there is no connection.
pub const NOT_CONNECTED: &str = "not connected to the LOUDAUTO engine";

/// Until the first connection: a retry every 250 ms for 5 s (from the start, or from
/// starting the engine), then every second.
const FAST_RETRY: Duration = Duration::from_millis(250);
const FAST_FOR: Duration = Duration::from_secs(5);
/// After that, and after a disconnect.
const RETRY: Duration = Duration::from_secs(1);
/// How long after a disconnect the link leaves the engine to systemd's own restart
/// (`RestartSec=2`) before starting it itself.
const RESTART_GRACE: Duration = Duration::from_secs(3);
/// How long after the engine asked the window to close the process ends anyway.
pub const CLOSE_BACKSTOP: Duration = Duration::from_secs(1);

/// What starts the engine when the link finds it isn't running.
enum Starter {
    /// Nothing: `start_at`, for tests.
    Never,
    /// `systemctl --user start loudauto.service`.
    Systemctl,
    /// Counts the starts, for the tests of when they happen.
    #[cfg(test)]
    Count(Arc<AtomicUsize>),
}

impl Starter {
    fn can_start(&self) -> bool {
        !matches!(self, Starter::Never)
    }

    async fn start(&self) {
        match self {
            Starter::Never => {}
            Starter::Systemctl => systemctl_start().await,
            #[cfg(test)]
            Starter::Count(starts) => {
                starts.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

/// What ends the process once [`CLOSE_BACKSTOP`] has passed since the engine asked the
/// window to close.
type Exit = Box<dyn FnOnce() + Send>;

/// What the link has for the window.
#[derive(Debug, PartialEq)]
pub enum LinkMsg {
    /// Connected, or reconnected: the engine's whole state, to draw from. Boxed, as in
    /// `ServerMsg::Welcome`, since it is far larger than the rest.
    Connected(Box<EngineState>),
    Event(Event),
    /// The answer to the request `send` returned `id` for; `Err` says why it failed.
    Reply {
        id: RequestId,
        result: Result<Reply, String>,
    },
    /// The connection is gone; the link is reconnecting.
    Disconnected,
}

/// What sends requests to the engine: the `Link`, or a fake in tests.
pub trait Requests {
    /// Sends `req`; its reply comes from the link's `poll`, under the id returned.
    fn send(&self, req: Request) -> RequestId;
}

/// The connection to the engine, for the UI thread.
pub struct Link {
    requests: async_mpsc::UnboundedSender<(RequestId, Request)>,
    messages: mpsc::Receiver<LinkMsg>,
    /// For answering a request the link's thread can no longer take.
    out: Out,
    next_id: Cell<RequestId>,
}

impl Link {
    /// Links to the engine at `engine_ipc::socket_path()`, starting it
    /// (`systemctl --user start loudauto.service`) if it isn't running. Ends the process
    /// should the window not close soon after the engine asks it to.
    pub fn start(ctx: egui::Context) -> Link {
        let exit = Box::new(|| {
            warn!(
                "the window didn't close within {} ms of the engine's asking; ending it",
                CLOSE_BACKSTOP.as_millis()
            );
            std::process::exit(0);
        });
        Self::spawn(engine_ipc::socket_path(), ctx, Starter::Systemctl, exit)
    }

    /// Links to the engine at `path`. Never starts one, nor ends the process: for tests.
    pub fn start_at(path: PathBuf, ctx: egui::Context) -> Link {
        Self::spawn(path, ctx, Starter::Never, Box::new(|| {}))
    }

    fn spawn(path: PathBuf, ctx: egui::Context, starter: Starter, exit: Exit) -> Link {
        let (requests, requests_rx) = async_mpsc::unbounded_channel();
        let (tx, messages) = mpsc::channel();
        let out = Out { tx, ctx };
        let thread_out = out.clone();
        let spawned = thread::Builder::new()
            .name("engine-link".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        runtime.block_on(run(path, starter, requests_rx, thread_out, exit));
                    }
                    Err(e) => warn!("engine link: no async runtime: {e}"),
                }
            });
        if let Err(e) = spawned {
            warn!("engine link: no thread: {e}");
        }
        Link {
            requests,
            messages,
            out,
            next_id: Cell::new(1),
        }
    }

    /// Sends `req`; its reply comes from `poll`, under the id returned.
    pub fn send(&self, req: Request) -> RequestId {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        if self.requests.send((id, req)).is_err() {
            // The link has stopped (the window is closing).
            self.out.reply(id, Err(NOT_CONNECTED.into()));
        }
        id
    }

    /// What came since the last call, in order. Never waits.
    pub fn poll(&mut self) -> Vec<LinkMsg> {
        self.messages.try_iter().collect()
    }
}

impl Requests for Link {
    fn send(&self, req: Request) -> RequestId {
        Link::send(self, req)
    }
}

/// The way back to the window: each message also asks it to repaint.
#[derive(Clone)]
struct Out {
    tx: mpsc::Sender<LinkMsg>,
    ctx: egui::Context,
}

impl Out {
    fn send(&self, msg: LinkMsg) {
        // Nobody to tell once the window has dropped its Link; the link ends with it.
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }

    fn reply(&self, id: RequestId, result: Result<Reply, String>) {
        self.send(LinkMsg::Reply { id, result });
    }
}

/// How a connection ended.
enum End {
    /// The engine went; reconnect.
    Lost,
    /// The engine asked the window to close; the link is done.
    Closing,
    /// The window dropped its Link.
    Dropped,
}

/// Connects, serves the connection, and reconnects, until the window is closing or gone.
/// The engine is started when it isn't running as the window opens, and once more after
/// each disconnect, no sooner than [`RESTART_GRACE`] after it.
async fn run(
    path: PathBuf,
    starter: Starter,
    mut requests: async_mpsc::UnboundedReceiver<(RequestId, Request)>,
    out: Out,
    exit: Exit,
) {
    let mut exit = Some(exit);
    let mut may_start = starter.can_start();
    let mut start_after: Option<Instant> = None;
    let mut fast_until = Some(Instant::now() + FAST_FOR);
    loop {
        let mut last_error = None;
        let (client, state, events) = loop {
            match EngineClient::connect_at(&path, Role::Window).await {
                Ok(connection) => break connection,
                Err(e) => {
                    let text = e.to_string();
                    if last_error.as_ref() != Some(&text) {
                        info!("engine link: {text}; retrying");
                        last_error = Some(text);
                    }
                    let due = start_after.is_none_or(|at| Instant::now() >= at);
                    if may_start && due && matches!(e, IpcError::NotRunning) {
                        may_start = false;
                        starter.start().await;
                        fast_until = Some(Instant::now() + FAST_FOR);
                    }
                }
            }
            let delay = match fast_until {
                Some(until) if Instant::now() < until => FAST_RETRY,
                _ => RETRY,
            };
            if !idle(delay, &mut requests, &out).await {
                return;
            }
        };
        info!("engine link: connected");
        // Started, or found running: systemd restarts it if it fails.
        fast_until = None;
        out.send(LinkMsg::Connected(Box::new(state)));
        match serve(Arc::new(client), events, &mut requests, &out, &mut exit).await {
            End::Lost => {
                info!("engine link: the engine went; reconnecting");
                out.send(LinkMsg::Disconnected);
                // If systemd doesn't bring it back (stopped, or past its start limit), the
                // window does, once.
                may_start = starter.can_start();
                start_after = Some(Instant::now() + RESTART_GRACE);
            }
            End::Closing | End::Dropped => return,
        }
    }
}

/// Waits `delay` with no connection, answering requests meanwhile. False once the window
/// has dropped its Link.
async fn idle(
    delay: Duration,
    requests: &mut async_mpsc::UnboundedReceiver<(RequestId, Request)>,
    out: &Out,
) -> bool {
    let wait = tokio::time::sleep(delay);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            () = &mut wait => return true,
            req = requests.recv() => match req {
                Some((id, _)) => out.reply(id, Err(NOT_CONNECTED.into())),
                None => return false,
            },
        }
    }
}

/// Forwards the engine's events and the window's requests, each request on a task of its
/// own so a slow reply holds up nothing. Once the engine has gone, the requests still out
/// fail (they do at once, the connection being closed) before this returns, so their
/// replies come before `Disconnected`. `CloseWindow` sets off `exit`, on a thread of its
/// own, [`CLOSE_BACKSTOP`] later.
async fn serve(
    client: Arc<EngineClient>,
    mut events: async_mpsc::UnboundedReceiver<Event>,
    requests: &mut async_mpsc::UnboundedReceiver<(RequestId, Request)>,
    out: &Out,
    exit: &mut Option<Exit>,
) -> End {
    let mut out_for = JoinSet::new();
    let mut closing = false;
    let end = loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(Event::CloseWindow) => {
                    // At once, even with the window's own Quit still waiting: the engine
                    // answers it only once the window is gone.
                    info!("engine link: the engine asked the window to close");
                    out.ctx
                        .send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
                    out.send(LinkMsg::Event(Event::CloseWindow));
                    closing = true;
                    // A thread, not a task: the link's runtime ends with the connection.
                    if let Some(exit) = exit.take() {
                        let backstop = thread::Builder::new()
                            .name("close-backstop".into())
                            .spawn(move || {
                                thread::sleep(CLOSE_BACKSTOP);
                                exit();
                            });
                        if let Err(e) = backstop {
                            warn!("engine link: no thread to end the window with: {e}");
                        }
                    }
                }
                Some(event) => out.send(LinkMsg::Event(event)),
                None if closing => break End::Closing,
                None => break End::Lost,
            },
            req = requests.recv() => match req {
                Some((id, req)) => {
                    debug!("engine link: request {id}: {req:?}");
                    let (client, out) = (client.clone(), out.clone());
                    out_for.spawn(async move {
                        let result = client.request(req).await.map_err(|e| e.to_string());
                        out.reply(id, result);
                    });
                }
                None => return End::Dropped,
            },
            // Answered requests: done.
            Some(_) = out_for.join_next(), if !out_for.is_empty() => {}
        }
    };
    while out_for.join_next().await.is_some() {}
    end
}

/// `systemctl --user start loudauto.service`, once, when the engine isn't running.
async fn systemctl_start() {
    info!("engine link: starting loudauto.service");
    let result = Command::new("systemctl")
        .args(["--user", "start", "loudauto.service"])
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            "engine link: systemctl --user start loudauto.service: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(e) => warn!("engine link: can't run systemctl: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        os::unix::net::UnixListener as StdListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread::JoinHandle,
        time::{Duration, Instant},
    };

    use eframe::egui::{RawInput, ViewportCommand, ViewportId};
    use engine_ipc::{
        ClientMsg, LoginStep, PROTOCOL_VERSION, PlayerState, Role, ServerMsg, SessionHealth,
        Settings, SoloistHealth,
        frame::{read_frame, write_frame},
    };
    use playback::{
        command::Repeat,
        event::{Actions, PlayStatus},
    };
    use tokio::{
        net::{UnixListener, UnixStream},
        sync::oneshot,
    };

    use super::*;

    /// How long a test waits for what it expects: roomy, for a loaded machine.
    const LIMIT: Duration = Duration::from_secs(10);

    /// An engine state told apart by `username`.
    fn state(username: &str) -> EngineState {
        EngineState {
            player: PlayerState {
                track: None,
                status: PlayStatus::Paused,
                active_here: true,
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
            username: Some(username.into()),
            soloist_expires_in_days: None,
        }
    }

    /// A socket path in a fresh temporary directory, and a listener bound to it.
    fn socket() -> (tempfile::TempDir, PathBuf, StdListener) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.sock");
        let listener = StdListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        (dir, path, listener)
    }

    /// The scripted engine: `script` on a runtime of its own, on a thread of its own.
    fn engine<F, Fut>(script: F) -> JoinHandle<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()>,
    {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(script());
        })
    }

    /// Accepts a window: its `Hello`, then a `Welcome` with `state`.
    async fn welcome(listener: &UnixListener, state: EngineState) -> UnixStream {
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello: ClientMsg = read_frame(&mut stream).await.unwrap().unwrap();
        assert_eq!(
            hello,
            ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                role: Role::Window,
            }
        );
        let welcome = ServerMsg::Welcome {
            version: PROTOCOL_VERSION,
            state: Box::new(state),
        };
        write_frame(&mut stream, &welcome).await.unwrap();
        stream
    }

    /// The next request on `stream`: its wire id and the request.
    async fn next_request(stream: &mut UnixStream) -> (u64, Request) {
        match read_frame(stream).await.unwrap().unwrap() {
            ClientMsg::Request { id, req } => (id, req),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    /// Polls `link` like the UI does, until `n` messages came or `LIMIT` passed.
    fn wait_for(link: &mut Link, n: usize) -> Vec<LinkMsg> {
        let deadline = Instant::now() + LIMIT;
        let mut got = Vec::new();
        while got.len() < n && Instant::now() < deadline {
            got.extend(link.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        got
    }

    /// A context that counts the repaints asked of it.
    fn counted_context() -> (egui::Context, Arc<AtomicUsize>) {
        let ctx = egui::Context::default();
        let repaints = Arc::new(AtomicUsize::new(0));
        let counter = repaints.clone();
        ctx.set_repaint_observer(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        (ctx, repaints)
    }

    /// Review Focus 1: the engine restarts while the window is open; the window is told,
    /// and reconnects by itself.
    #[test]
    fn link_reconnects_after_engine_restart() {
        let (_dir, path, listener) = socket();
        let (stop, stopped) = oneshot::channel::<()>();
        let engine_path = path.clone();
        let engine = engine(move || async move {
            let listener = UnixListener::from_std(listener).unwrap();
            // The first engine answers the window, then exits.
            drop(welcome(&listener, state("first")).await);
            drop(listener);
            std::fs::remove_file(&engine_path).unwrap();
            // systemd starts it again (RestartSec=2).
            tokio::time::sleep(Duration::from_secs(2)).await;
            let listener = UnixListener::bind(&engine_path).unwrap();
            let _window = welcome(&listener, state("second")).await;
            let _ = stopped.await;
        });

        let (ctx, repaints) = counted_context();
        let mut link = Link::start_at(path, ctx);
        let got = wait_for(&mut link, 3);
        assert_eq!(
            got,
            vec![
                LinkMsg::Connected(Box::new(state("first"))),
                LinkMsg::Disconnected,
                LinkMsg::Connected(Box::new(state("second"))),
            ]
        );
        // Each message asked for a repaint.
        assert!(repaints.load(Ordering::SeqCst) >= 3);
        let _ = stop.send(());
        engine.join().unwrap();
    }

    /// A request with no engine is answered at once; one sent once connected gets the
    /// engine's answer. Both under the link's ids, which aren't the wire's.
    #[test]
    fn replies_carry_the_link_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.sock");
        let (ctx, _) = counted_context();
        let mut link = Link::start_at(path.clone(), ctx);

        let early = link.send(Request::Play);
        let started = Instant::now();
        assert_eq!(
            wait_for(&mut link, 1),
            vec![LinkMsg::Reply {
                id: early,
                result: Err("not connected to the LOUDAUTO engine".into()),
            }]
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );

        let (stop, stopped) = oneshot::channel::<()>();
        let (wire_tx, wire_rx) = std::sync::mpsc::channel();
        let listener = StdListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let engine = engine(move || async move {
            let listener = UnixListener::from_std(listener).unwrap();
            let mut window = welcome(&listener, state("e")).await;
            let (id, req) = next_request(&mut window).await;
            assert_eq!(req, Request::Pause);
            let reply = ServerMsg::Reply {
                id,
                result: Err("The player is restarting.".into()),
            };
            write_frame(&mut window, &reply).await.unwrap();
            wire_tx.send(id).unwrap();
            let _ = stopped.await;
        });

        assert_eq!(
            wait_for(&mut link, 1),
            vec![LinkMsg::Connected(Box::new(state("e")))]
        );
        let late = link.send(Request::Pause);
        assert_ne!(late, early);
        assert_eq!(
            wait_for(&mut link, 1),
            vec![LinkMsg::Reply {
                id: late,
                result: Err("The player is restarting.".into()),
            }]
        );
        assert_ne!(wire_rx.recv_timeout(LIMIT).unwrap(), late);
        let _ = stop.send(());
        engine.join().unwrap();
    }

    /// Final review m3: after a disconnect, an engine that isn't running is started once
    /// more (only after systemd's own restart has had `RESTART_GRACE`), as it was when the
    /// window opened; not again while it stays down.
    #[test]
    fn a_stopped_engine_is_started_once_more() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.sock");
        let starts = Arc::new(AtomicUsize::new(0));
        let (ctx, _) = counted_context();
        let mut link = Link::spawn(
            path.clone(),
            ctx,
            Starter::Count(starts.clone()),
            Box::new(|| {}),
        );
        // Not running when the window opens: started, as always.
        let deadline = Instant::now() + LIMIT;
        while starts.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);

        let listener = StdListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let engine_path = path.clone();
        let engine = engine(move || async move {
            let listener = UnixListener::from_std(listener).unwrap();
            drop(welcome(&listener, state("e")).await);
            // Stopped for good: the socket goes, and systemd won't bring it back.
            drop(listener);
            std::fs::remove_file(&engine_path).unwrap();
        });
        assert_eq!(
            wait_for(&mut link, 2),
            vec![
                LinkMsg::Connected(Box::new(state("e"))),
                LinkMsg::Disconnected
            ]
        );
        engine.join().unwrap();
        let gone = Instant::now();
        let deadline = gone + LIMIT;
        while starts.load(Ordering::SeqCst) == 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(starts.load(Ordering::SeqCst), 2, "started once more");
        assert!(gone.elapsed() >= RESTART_GRACE - Duration::from_millis(100));
        std::thread::sleep(RETRY * 2);
        assert_eq!(starts.load(Ordering::SeqCst), 2, "only once");
    }

    /// Final review I3 (the Task 22 carry): once the engine has asked the window to
    /// close, the process ends within `CLOSE_BACKSTOP` even if egui never runs a pass
    /// again (a minimized window on KWin), so a second window never stays beside it.
    #[test]
    fn close_window_ends_the_process_even_without_a_frame() {
        let (_dir, path, listener) = socket();
        let engine = engine(move || async move {
            let listener = UnixListener::from_std(listener).unwrap();
            let mut window = welcome(&listener, state("e")).await;
            let close = ServerMsg::Event(Event::CloseWindow);
            write_frame(&mut window, &close).await.unwrap();
            // The window's process ends; the link never hangs up by itself here.
            tokio::time::sleep(LIMIT).await;
        });
        let (exited_tx, exited) = std::sync::mpsc::channel();
        let (ctx, _) = counted_context();
        // The UI never runs: nothing polls the link, no pass closes the viewport.
        let _link = Link::spawn(
            path,
            ctx,
            Starter::Never,
            Box::new(move || exited_tx.send(Instant::now()).unwrap()),
        );
        let started = Instant::now();
        exited
            .recv_timeout(LIMIT)
            .expect("the process was never ended");
        assert!(
            started.elapsed() >= CLOSE_BACKSTOP,
            "{:?}",
            started.elapsed()
        );
        drop(engine);
    }

    /// Task 20's carry: `CloseWindow` closes the window at once, though the window's own
    /// Quit is still waiting for its reply; and the link doesn't reconnect after it.
    #[test]
    fn close_window_acts_while_quit_waits() {
        let (_dir, path, listener) = socket();
        let (reconnected_tx, reconnected_rx) = std::sync::mpsc::channel();
        let engine = engine(move || async move {
            let listener = UnixListener::from_std(listener).unwrap();
            let mut window = welcome(&listener, state("e")).await;
            let (_, req) = next_request(&mut window).await;
            assert_eq!(req, Request::Quit);
            // Quit closes the window first, and answers only after.
            let close = ServerMsg::Event(Event::CloseWindow);
            write_frame(&mut window, &close).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(window);
            let again = tokio::time::timeout(Duration::from_millis(1500), listener.accept()).await;
            reconnected_tx.send(again.is_ok()).unwrap();
        });

        let (ctx, _) = counted_context();
        let mut link = Link::start_at(path, ctx.clone());
        assert_eq!(
            wait_for(&mut link, 1),
            vec![LinkMsg::Connected(Box::new(state("e")))]
        );
        let quit = link.send(Request::Quit);
        assert_eq!(
            wait_for(&mut link, 1),
            vec![LinkMsg::Event(Event::CloseWindow)]
        );
        let mut out = ctx.run_ui(RawInput::default(), |_| {});
        out.textures_delta.clear();
        assert!(
            out.viewport_output[&ViewportId::ROOT]
                .commands
                .contains(&ViewportCommand::Close)
        );

        assert!(
            !reconnected_rx.recv_timeout(LIMIT).unwrap(),
            "the link reconnected"
        );
        engine.join().unwrap();
        // The Quit fails with the connection; there is no Disconnected for a closing window.
        assert_eq!(
            link.poll(),
            vec![LinkMsg::Reply {
                id: quit,
                result: Err("the engine closed the connection".into()),
            }]
        );
    }
}
