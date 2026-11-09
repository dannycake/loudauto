//! The `Playback` trait every consumer uses, and its Soloist implementation.

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::{
    Result,
    client::SoloistClient,
    command::{Command, Repeat, repeat_commands, set_volume},
    config::{KeyCache, SoloistConfig, op_read_key, resolve_api_key},
    event::PlaybackEvent,
    supervisor::{Supervisor, running_soloist},
};

#[async_trait]
pub trait Playback: Send + Sync {
    async fn play(&self, uri: Option<&str>) -> Result<()>;
    async fn pause(&self) -> Result<()>;
    async fn next(&self) -> Result<()>;
    async fn prev(&self) -> Result<()>;
    async fn seek(&self, position_ms: u64) -> Result<()>;
    async fn set_volume(&self, volume: u8) -> Result<()>;
    async fn set_shuffle(&self, on: bool) -> Result<()>;
    async fn set_repeat(&self, mode: Repeat) -> Result<()>;
    async fn add_to_queue(&self, uri: &str) -> Result<()>;
    async fn activate(&self) -> Result<()>;
    /// Ask for the full current state (auth, playback, next 10 queued).
    async fn refresh(&self) -> Result<()>;
    fn subscribe(&self) -> broadcast::Receiver<PlaybackEvent>;
}

/// Starting a specific URI makes this device the active player first.
pub fn play_commands(uri: Option<&str>) -> Vec<Command> {
    match uri {
        Some(uri) => vec![
            Command::Activate,
            Command::Play {
                uri: Some(uri.to_string()),
            },
        ],
        None => vec![Command::Play { uri: None }],
    }
}

/// Soloist's reply to a command sent before it has logged in.
pub const AUTH_REFUSAL: &str = "command requires authentication";

/// Soloist opens its WebSocket ~200 ms before it finishes logging in, and refuses most
/// commands until then. Hold startup actions until the first logged-in `Auth` event.
#[derive(Debug, Default)]
pub struct LoginGate {
    open: bool,
}

impl LoginGate {
    /// True exactly once: on the first `Auth { logged_in: true }`.
    pub fn opens(&mut self, event: &PlaybackEvent) -> bool {
        let first = !self.open && matches!(event, PlaybackEvent::Auth { logged_in: true });
        self.open |= first;
        first
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Refusals before login are expected startup noise, not errors worth showing.
    pub fn hides(&self, event: &PlaybackEvent) -> bool {
        !self.open && matches!(event, PlaybackEvent::Error(m) if m == AUTH_REFUSAL)
    }
}

/// Broadcast capacity; a slow consumer that falls this far behind sees `Lagged`.
const EVENT_CAPACITY: usize = 256;

pub struct SoloistBackend {
    supervisor: Supervisor,
    client: SoloistClient,
    events: broadcast::Sender<PlaybackEvent>,
    /// Subscribed before Soloist started, for the first `subscribe`: Soloist logs its expiry
    /// at startup, before anyone else could subscribe.
    first: std::sync::Mutex<Option<broadcast::Receiver<PlaybackEvent>>>,
}

impl SoloistBackend {
    /// Start Soloist and connect. Subscribe, then call `refresh` for the initial state; the
    /// first subscriber also gets what Soloist logged while starting (`ExpiresInDays`).
    /// The stream ends with one `Exited` when Soloist ends, whatever ended it.
    /// The key comes from `SOLOIST_API_KEY`, `cache`, or `op_read_key` (see `resolve_api_key`).
    pub async fn launch(config: SoloistConfig, cache: &dyn KeyCache) -> Result<SoloistBackend> {
        let env = std::env::var("SOLOIST_API_KEY").ok();
        Self::launch_with_key(config, resolve_api_key(env, cache, op_read_key)).await
    }

    /// `launch` with an injectable key source (tests). `key` is only awaited once the
    /// binary is known to exist and no other Soloist holds the data dir.
    pub async fn launch_with_key(
        config: SoloistConfig,
        key: impl std::future::Future<Output = Result<String>>,
    ) -> Result<SoloistBackend> {
        // Check the binary before asking for the key: a missing Soloist should say
        // "download it", not look for a key first.
        if !config.binary.is_file() {
            return Err(crate::SoloistError::BinaryMissing(config.binary.clone()));
        }
        if let Some(pid) = running_soloist(&config.data_dir) {
            return Err(crate::SoloistError::AlreadyRunning(pid));
        }
        let key = key.await?;
        let (events, first) = broadcast::channel(EVENT_CAPACITY);
        let supervisor = Supervisor::start(&config, &key, events.clone()).await?;
        match SoloistClient::connect(supervisor.ws_port(), events.clone()).await {
            Ok(client) => Ok(SoloistBackend {
                supervisor,
                client,
                events,
                first: std::sync::Mutex::new(Some(first)),
            }),
            Err(e) => {
                let _ = supervisor.stop().await;
                Err(e)
            }
        }
    }

    /// Stop Soloist; subscribers see its `Exited`.
    pub async fn shutdown(self) -> Result<()> {
        self.supervisor.stop().await
    }

    async fn send_all(&self, commands: impl IntoIterator<Item = Command>) -> Result<()> {
        for cmd in commands {
            self.client.send(cmd).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Playback for SoloistBackend {
    async fn play(&self, uri: Option<&str>) -> Result<()> {
        self.send_all(play_commands(uri)).await
    }
    async fn pause(&self) -> Result<()> {
        self.client.send(Command::Pause).await
    }
    async fn next(&self) -> Result<()> {
        self.client.send(Command::SkipNext).await
    }
    async fn prev(&self) -> Result<()> {
        self.client.send(Command::SkipPrev).await
    }
    async fn seek(&self, position_ms: u64) -> Result<()> {
        self.client.send(Command::Seek { position_ms }).await
    }
    async fn set_volume(&self, volume: u8) -> Result<()> {
        self.client.send(set_volume(volume)).await
    }
    async fn set_shuffle(&self, on: bool) -> Result<()> {
        self.client.send(Command::SetShuffle { enabled: on }).await
    }
    async fn set_repeat(&self, mode: Repeat) -> Result<()> {
        self.send_all(repeat_commands(mode)).await
    }
    async fn add_to_queue(&self, uri: &str) -> Result<()> {
        self.client
            .send(Command::AddToQueue {
                uri: uri.to_string(),
            })
            .await
    }
    async fn activate(&self) -> Result<()> {
        self.client.send(Command::Activate).await
    }
    async fn refresh(&self) -> Result<()> {
        self.send_all([
            Command::GetAuthState,
            Command::GetState,
            Command::GetQueue { limit: Some(10) },
        ])
        .await
    }
    fn subscribe(&self) -> broadcast::Receiver<PlaybackEvent> {
        let first = self.first.lock().ok().and_then(|mut first| first.take());
        first.unwrap_or_else(|| self.events.subscribe())
    }
}

/// Connect to a Soloist that is already running with `config`'s data dir.
pub async fn connect_running(
    config: &SoloistConfig,
    events: broadcast::Sender<PlaybackEvent>,
) -> Result<SoloistClient> {
    let text = tokio::fs::read_to_string(config.data_dir.join("ws.port"))
        .await
        .map_err(|_| {
            crate::SoloistError::WebSocket("no running Soloist (ws.port missing)".into())
        })?;
    let port = text
        .trim()
        .parse()
        .map_err(|_| crate::SoloistError::WebSocket(format!("bad ws.port contents: {text:?}")))?;
    SoloistClient::connect(port, events).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn _is_object_safe(b: SoloistBackend) -> Box<dyn Playback> {
        Box::new(b)
    }

    fn auth(logged_in: bool) -> PlaybackEvent {
        PlaybackEvent::Auth { logged_in }
    }

    #[test]
    fn login_gate_opens_once_on_first_login() {
        let mut gate = LoginGate::default();
        assert!(!gate.opens(&auth(false)));
        assert!(!gate.opens(&PlaybackEvent::Volume(40)));
        assert!(gate.opens(&auth(true)));
        assert!(!gate.opens(&auth(true)));
        assert!(gate.is_open());
    }

    #[test]
    fn login_gate_hides_auth_refusals_only_before_login() {
        let refusal = PlaybackEvent::Error(AUTH_REFUSAL.to_string());
        let other = PlaybackEvent::Error("track unavailable".into());
        let mut gate = LoginGate::default();
        assert!(gate.hides(&refusal));
        assert!(!gate.hides(&other));
        gate.opens(&auth(true));
        assert!(!gate.hides(&refusal));
    }

    #[tokio::test]
    async fn already_running_does_not_ask_for_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = SoloistConfig::from_vars(|_| None);
        config.binary =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-soloist.sh");
        config.data_dir = dir.path().join("data");
        config.cache_dir = dir.path().join("cache");
        let (tx, _rx) = broadcast::channel(16);
        let first = Supervisor::start(&config, "KEY", tx).await.unwrap();
        let got = SoloistBackend::launch_with_key(config, async {
            panic!("asked 1Password for the key")
        })
        .await;
        assert!(matches!(got, Err(crate::SoloistError::AlreadyRunning(_))));
        first.stop().await.unwrap();
    }

    /// Soloist logs its expiry before its WebSocket opens, so before `launch` returns.
    #[tokio::test]
    async fn first_subscriber_sees_startup_lines_and_the_exit() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });
        let mut config = SoloistConfig::from_vars(|_| None);
        config.binary =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-soloist.sh");
        config.data_dir = dir.path().join("data");
        config.cache_dir = dir.path().join("cache");
        config.env = vec![
            ("FAKE_MODE".into(), "nonutf8".into()),
            ("FAKE_PORT".into(), port.to_string()),
        ];
        let backend = SoloistBackend::launch_with_key(config, async { Ok("KEY".to_string()) })
            .await
            .unwrap();
        let _ws = server.await.unwrap();
        let mut events = backend.subscribe();
        let mut next = async || {
            tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
                .await
                .expect("no event")
                .unwrap()
        };
        assert_eq!(next().await, PlaybackEvent::ExpiresInDays(89));
        backend.shutdown().await.unwrap();
        assert!(matches!(
            next().await,
            PlaybackEvent::Exited { code: Some(0), .. }
        ));
    }

    #[tokio::test]
    async fn missing_binary_does_not_ask_for_key() {
        let mut config = SoloistConfig::from_vars(|_| None);
        config.binary = "/nonexistent/soloist".into();
        let got = SoloistBackend::launch_with_key(config, async {
            panic!("asked 1Password for the key")
        })
        .await;
        assert!(matches!(got, Err(crate::SoloistError::BinaryMissing(_))));
    }

    #[test]
    fn play_uri_activates_first() {
        assert_eq!(
            play_commands(Some("spotify:playlist:21erFbLzdJurEYKz86wDwv")),
            vec![
                Command::Activate,
                Command::Play {
                    uri: Some("spotify:playlist:21erFbLzdJurEYKz86wDwv".into())
                }
            ]
        );
        assert_eq!(play_commands(None), vec![Command::Play { uri: None }]);
    }
}
