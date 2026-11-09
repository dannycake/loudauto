//! The client the window and `loudauto-cli` use to reach the engine.
//!
//! One task reads the engine's frames and routes them: each `Reply` to the request
//! waiting on its id, each `Event` to the events channel. Another writes the requests,
//! so a caller that gives up on `request` can never leave half a frame on the socket.

use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

use crate::{
    PROTOCOL_VERSION,
    frame::{read_frame, write_frame},
    proto::{ClientMsg, EngineState, Event, Reply, Request, Role, ServerMsg},
};

/// How long `connect` waits for the engine's `Welcome`.
pub const WELCOME_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `request` waits for its reply, unless [`timeout_for`] gives the request
/// longer.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `request` waits for a page, lyrics or credits: past the engine's own 15 s
/// wait for Spotify (`engine::pages::UNCACHED_LIMIT`).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `request` waits for a context play: past the engine's 20 s for it
/// (`engine::requests::PLAY_CEILING`).
pub const PLAY_TIMEOUT: Duration = Duration::from_secs(25);
/// How long `request` waits for a radio: past the engine's 30 s for finding and playing
/// it (`engine::requests::RADIO_CEILING`).
pub const RADIO_TIMEOUT: Duration = Duration::from_secs(35);
/// How long `request` waits for a heart: past the engine's 25 s for the presses before
/// it and 20 s for Spotify (`engine::heart::TURN_CEILING`, `WRITE_CEILING`).
pub const HEART_TIMEOUT: Duration = Duration::from_secs(50);

/// How long `request` waits for `req`'s reply: past the longest the engine itself
/// waits before answering, so the client never drops a reply the engine still sends.
pub fn timeout_for(req: &Request) -> Duration {
    match req {
        Request::Page { .. } | Request::Lyrics { .. } | Request::Credits { .. } => FETCH_TIMEOUT,
        Request::PlayContext { .. } => PLAY_TIMEOUT,
        Request::StartRadio { .. } => RADIO_TIMEOUT,
        Request::ToggleHeart { .. } | Request::SetLiked { .. } => HEART_TIMEOUT,
        _ => REQUEST_TIMEOUT,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("the LOUDAUTO engine is not running")]
    NotRunning,
    #[error("the engine speaks protocol {engine}, this build speaks {ours}")]
    VersionMismatch { engine: u32, ours: u32 },
    #[error("the engine closed the connection")]
    Closed,
    #[error("the engine didn't answer in time")]
    Timeout,
    /// The engine answered the request with this error.
    #[error("{0}")]
    Engine(String),
    #[error("engine socket: {0}")]
    Io(#[from] io::Error),
}

type Waiter = oneshot::Sender<Result<Reply, String>>;

/// Requests waiting for their reply, by id. `None` once the connection is gone, so a
/// request made after that fails at once instead of waiting out its timeout.
type Pending = Arc<Mutex<Option<HashMap<u64, Waiter>>>>;

/// A connection to the engine. Dropping it closes the connection and ends the events
/// stream.
#[derive(Debug)]
pub struct EngineClient {
    outgoing: mpsc::UnboundedSender<ClientMsg>,
    pending: Pending,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
}

impl EngineClient {
    /// Connect to the engine at `socket_path()`. Returns the client, the engine's state
    /// as of now, and the events that follow it; the events end when the engine goes.
    pub async fn connect(
        role: Role,
    ) -> Result<(EngineClient, EngineState, mpsc::UnboundedReceiver<Event>), IpcError> {
        Self::connect_at(crate::socket_path(), role).await
    }

    /// `connect` to the socket at `path`.
    pub async fn connect_at(
        path: impl AsRef<Path>,
        role: Role,
    ) -> Result<(EngineClient, EngineState, mpsc::UnboundedReceiver<Event>), IpcError> {
        let stream = UnixStream::connect(path.as_ref())
            .await
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => IpcError::NotRunning,
                _ => IpcError::Io(e),
            })?;
        let (mut read, mut write) = stream.into_split();
        let state = timeout(WELCOME_TIMEOUT, handshake(&mut read, &mut write, role))
            .await
            .map_err(|_| IpcError::Timeout)??;

        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        tokio::spawn(write_loop(write, outgoing_rx, pending.clone()));
        let reader = tokio::spawn(read_loop(read, pending.clone(), events_tx));
        let client = EngineClient {
            outgoing,
            pending,
            next_id: AtomicU64::new(1),
            reader,
        };
        Ok((client, state, events))
    }

    /// Send `req` and wait for its reply, as long as [`timeout_for`] says. An error the
    /// engine answered with is `IpcError::Engine`.
    pub async fn request(&self, req: Request) -> Result<Reply, IpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (waiter, reply) = oneshot::channel();
        match lock(&self.pending).as_mut() {
            Some(waiting) => waiting.insert(id, waiter),
            None => return Err(IpcError::Closed),
        };
        // However this ends (a reply, a timeout, the caller giving up), the entry goes.
        let _forget = Forget {
            pending: &self.pending,
            id,
        };
        let wait = timeout_for(&req);
        self.outgoing
            .send(ClientMsg::Request { id, req })
            .map_err(|_| IpcError::Closed)?;
        match timeout(wait, reply).await {
            Err(_) => Err(IpcError::Timeout),
            Ok(Err(_)) => Err(IpcError::Closed),
            Ok(Ok(result)) => result.map_err(IpcError::Engine),
        }
    }
}

impl Drop for EngineClient {
    fn drop(&mut self) {
        // The writer stops by itself once `outgoing` is gone.
        self.reader.abort();
    }
}

/// Removes a request's entry from `pending` when dropped.
struct Forget<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        if let Some(waiting) = lock(self.pending).as_mut() {
            waiting.remove(&self.id);
        }
    }
}

fn lock(pending: &Pending) -> MutexGuard<'_, Option<HashMap<u64, Waiter>>> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Fail every waiting request, and every later one, with `Closed`.
fn close(pending: &Pending) {
    lock(pending).take();
}

/// `Hello`, then the engine's `Welcome`.
async fn handshake(
    read: &mut OwnedReadHalf,
    write: &mut OwnedWriteHalf,
    role: Role,
) -> Result<EngineState, IpcError> {
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        role,
    };
    write_frame(write, &hello).await.map_err(hangup)?;
    // The version is read before the rest: an engine speaking another protocol may send
    // a state this build can't parse, and the mismatch is what the user needs to hear.
    let welcome: serde_json::Value = read_frame(read)
        .await
        .map_err(hangup)?
        .ok_or(IpcError::Closed)?;
    if welcome["t"] == "Welcome"
        && let Some(version) = welcome["c"]["version"].as_u64()
        && version != u64::from(PROTOCOL_VERSION)
    {
        return Err(IpcError::VersionMismatch {
            engine: u32::try_from(version).unwrap_or(u32::MAX),
            ours: PROTOCOL_VERSION,
        });
    }
    match serde_json::from_value(welcome) {
        Ok(ServerMsg::Welcome { state, .. }) => Ok(*state),
        Ok(_) => Err(invalid("the engine's first message wasn't Welcome")),
        Err(e) => Err(invalid(&format!("the engine's Welcome: {e}"))),
    }
}

/// An I/O error during the handshake; the engine hanging up is `Closed`.
fn hangup(e: io::Error) -> IpcError {
    match e.kind() {
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::UnexpectedEof => IpcError::Closed,
        _ => IpcError::Io(e),
    }
}

fn invalid(what: &str) -> IpcError {
    IpcError::Io(io::Error::new(io::ErrorKind::InvalidData, what.to_string()))
}

async fn write_loop(
    mut write: OwnedWriteHalf,
    mut outgoing: mpsc::UnboundedReceiver<ClientMsg>,
    pending: Pending,
) {
    while let Some(msg) = outgoing.recv().await {
        if let Err(e) = write_frame(&mut write, &msg).await {
            log::warn!("engine connection: writing failed: {e}");
            close(&pending);
            return;
        }
    }
}

/// Route the engine's frames until it hangs up; then fail what still waits and end the
/// events stream (by dropping `events`).
async fn read_loop(
    mut read: OwnedReadHalf,
    pending: Pending,
    events: mpsc::UnboundedSender<Event>,
) {
    loop {
        match read_frame::<_, ServerMsg>(&mut read).await {
            Ok(Some(ServerMsg::Reply { id, result })) => {
                let waiter = lock(&pending).as_mut().and_then(|w| w.remove(&id));
                // No waiter: the request timed out or its caller gave up.
                if let Some(waiter) = waiter {
                    let _ = waiter.send(result);
                }
            }
            Ok(Some(ServerMsg::Event(event))) => {
                // Nobody listening for events is fine; replies still need routing.
                let _ = events.send(event);
            }
            Ok(Some(ServerMsg::Welcome { .. })) => {
                log::warn!("engine connection: a second Welcome, ignored");
            }
            Ok(None) => break,
            Err(e) => {
                log::warn!("engine connection: reading failed: {e}");
                break;
            }
        }
    }
    close(&pending);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::PageRequest;

    /// Pages, lyrics and credits wait for Spotify up to the engine's 15 s, a context
    /// play up to its 20 s, a radio up to 30 s and a heart up to 45 s (its turn, then
    /// Spotify): the client waits longer than each (the engine's own tests check the
    /// bounds); the rest 10 s.
    #[test]
    fn timeouts_outlast_the_engine() {
        let secs = |req: Request| timeout_for(&req).as_secs();
        let uri = || "spotify:track:t".to_string();
        assert_eq!(
            secs(Request::Page {
                req: PageRequest::Home,
                prefetch: false
            }),
            20
        );
        assert_eq!(
            secs(Request::Page {
                req: PageRequest::Liked,
                prefetch: true
            }),
            20
        );
        assert_eq!(secs(Request::Lyrics { track_uri: uri() }), 20);
        assert_eq!(secs(Request::Credits { track_uri: uri() }), 20);
        let play = Request::PlayContext {
            context_uri: "spotify:playlist:p".into(),
            track_uri: None,
            track_uid: None,
            shuffle: None,
        };
        assert_eq!(secs(play), 25);
        assert_eq!(secs(Request::StartRadio { seed_uri: uri() }), 35);
        assert_eq!(secs(Request::ToggleHeart { uri: None }), 50);
        assert_eq!(secs(Request::ToggleHeart { uri: Some(uri()) }), 50);
        let set = Request::SetLiked {
            uri: uri(),
            liked: true,
        };
        assert_eq!(
            timeout_for(&set),
            timeout_for(&Request::ToggleHeart { uri: None })
        );
        for quick in [
            Request::Play,
            Request::Next,
            Request::Quit,
            Request::LikedState { uris: vec![uri()] },
            Request::PlayUri { uri: uri() },
        ] {
            assert_eq!(timeout_for(&quick), REQUEST_TIMEOUT, "{quick:?}");
        }
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(10));
    }
}
