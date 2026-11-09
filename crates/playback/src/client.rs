//! WebSocket connection to a running Soloist (`ws://127.0.0.1:<port>`).

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    Result, SoloistError,
    command::Command,
    event::{PlaybackEvent, normalize},
    wire::parse_incoming,
};

/// Commands waiting to be written; Soloist answers fast, so a small buffer is plenty.
const COMMAND_BUFFER: usize = 64;

/// The `PlaybackEvent::Error` sent when the WebSocket closes. Soloist closes it when it
/// exits, usually just before its `Exited` arrives.
pub const CONNECTION_CLOSED: &str = "Soloist connection closed";

pub struct SoloistClient {
    commands: mpsc::Sender<String>,
}

impl SoloistClient {
    /// Connect and start forwarding Soloist's messages, normalized, into `events`.
    pub async fn connect(
        port: u16,
        events: broadcast::Sender<PlaybackEvent>,
    ) -> Result<SoloistClient> {
        let (ws, _) = connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .map_err(|e| SoloistError::WebSocket(e.to_string()))?;
        let (mut sink, mut stream) = ws.split();
        let (commands, mut queue) = mpsc::channel::<String>(COMMAND_BUFFER);

        let writer = tokio::spawn(async move {
            while let Some(json) = queue.recv().await {
                if sink.send(Message::text(json)).await.is_err() {
                    break;
                }
            }
        });
        let stop_writer = writer.abort_handle();
        tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(Message::Text(text)) => {
                        // Unknown and malformed frames are skipped; the connection survives.
                        for event in parse_incoming(text.as_str())
                            .map(normalize)
                            .unwrap_or_default()
                        {
                            let _ = events.send(event);
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            // Dropping the writer's queue makes every later `send` fail with `Closed`.
            stop_writer.abort();
            let _ = events.send(PlaybackEvent::Error(CONNECTION_CLOSED.into()));
        });
        Ok(SoloistClient { commands })
    }

    pub async fn send(&self, cmd: Command) -> Result<()> {
        self.commands
            .send(cmd.to_json())
            .await
            .map_err(|_| SoloistError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use tokio::{net::TcpListener, task::JoinHandle};
    use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};

    use super::*;

    const CAPTURE: &str = include_str!("../tests/fixtures/state.json");

    type Server = WebSocketStream<tokio::net::TcpStream>;

    async fn server() -> (u16, JoinHandle<Server>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept_async(stream).await.unwrap()
        });
        (port, accept)
    }

    async fn setup() -> (SoloistClient, Server, broadcast::Receiver<PlaybackEvent>) {
        let (port, accept) = server().await;
        let (tx, rx) = broadcast::channel(64);
        let client = SoloistClient::connect(port, tx).await.unwrap();
        (client, accept.await.unwrap(), rx)
    }

    async fn next(rx: &mut broadcast::Receiver<PlaybackEvent>) -> PlaybackEvent {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn receives_normalized_events() {
        let (_client, mut server, mut rx) = setup().await;
        let raw: Vec<serde_json::Value> = serde_json::from_str(CAPTURE).unwrap();
        server
            .send(Message::text(raw[0].to_string()))
            .await
            .unwrap();
        server
            .send(Message::text(raw[1].to_string()))
            .await
            .unwrap();
        assert_eq!(next(&mut rx).await, PlaybackEvent::Auth { logged_in: true });
        assert_eq!(next(&mut rx).await, PlaybackEvent::Active(false));
        assert_eq!(next(&mut rx).await, PlaybackEvent::Active(false));
        assert!(
            matches!(next(&mut rx).await, PlaybackEvent::Track(t) if t.title == "Dancing with your eyes closed")
        );
    }

    #[tokio::test]
    async fn sends_commands() {
        let (client, mut server, _rx) = setup().await;
        client.send(Command::Pause).await.unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(2), server.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let got: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(got, serde_json::json!({"type":"command","command":"pause"}));
    }

    #[tokio::test]
    async fn survives_garbage_and_unknown() {
        let (_client, mut server, mut rx) = setup().await;
        server.send(Message::text("{not json")).await.unwrap();
        server
            .send(Message::text(r#"{"type":"options_changed","options":{}}"#))
            .await
            .unwrap();
        server
            .send(Message::text(r#"{"type":"something_new"}"#))
            .await
            .unwrap();
        server
            .send(Message::text(r#"{"type":"volume_changed","volume":7}"#))
            .await
            .unwrap();
        assert_eq!(next(&mut rx).await, PlaybackEvent::Volume(7));
    }

    #[tokio::test]
    async fn error_frame_becomes_event() {
        let (_client, mut server, mut rx) = setup().await;
        server
            .send(Message::text(
                r#"{"type":"error","message":"command requires authentication"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut rx).await,
            PlaybackEvent::Error("command requires authentication".into())
        );
    }

    #[tokio::test]
    async fn close_is_reported() {
        let (client, mut server, mut rx) = setup().await;
        server.close(None).await.unwrap();
        assert_eq!(
            next(&mut rx).await,
            PlaybackEvent::Error("Soloist connection closed".into())
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            match client.send(Command::GetState).await {
                Err(SoloistError::Closed) => break,
                _ if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                other => panic!("send after close: {other:?}"),
            }
        }
    }
}
