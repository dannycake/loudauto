//! Starting a list at a chosen track on Soloist: the Connect play command Spotify's web
//! player sends, addressed to Soloist's device. Soloist's own `play` takes only a URI
//! and always starts at the first track.

use std::fmt::Write as _;

use rand::RngCore;
use serde_json::{Map, Value, json};

use super::{Web, lanes::Lane};
use crate::Result;

/// What to play: a context (album, playlist, Liked Songs), from one of its tracks or from
/// the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectPlay {
    pub context_uri: String,
    /// The track to start at; `None` starts the context from the top.
    pub track_uri: Option<String>,
    /// Which copy of the track, for a playlist that holds it more than once.
    pub track_uid: Option<String>,
    pub shuffle: bool,
}

/// The play command's body. `skip_to` holds only the fields of `p` that are set, so it
/// is `{}` when neither is.
pub fn connect_play_body(p: &ConnectPlay, feature_version: &str, command_id: &str) -> Value {
    let mut skip_to = Map::new();
    if let Some(uri) = &p.track_uri {
        skip_to.insert("track_uri".into(), uri.as_str().into());
    }
    if let Some(uid) = &p.track_uid {
        skip_to.insert("track_uid".into(), uid.as_str().into());
    }
    json!({"command": {
        "endpoint": "play",
        "context": {
            "uri": p.context_uri,
            "url": format!("context://{}", p.context_uri),
            "metadata": {},
        },
        "play_origin": {"feature_identifier": "loudauto", "feature_version": feature_version},
        "options": {
            "license": "premium",
            "skip_to": skip_to,
            "player_options_override": {"shuffling_context": p.shuffle},
        },
        "logging_params": {"command_id": command_id},
    }})
}

/// `bytes` random bytes as lowercase hex, from the OS-seeded thread RNG: a Connect
/// `from` id (20 bytes) or command id (16).
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    let mut hex = String::with_capacity(bytes * 2);
    for b in buf {
        // Writing to a String can't fail.
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

impl Web {
    /// Sends `p` to device `to_device` as controller `from_device`, on the User lane.
    /// Spotify answers with an ack; the play itself shows in the device's own events.
    pub async fn connect_play(
        &self,
        from_device: &str,
        to_device: &str,
        p: &ConnectPlay,
    ) -> Result<()> {
        let body = connect_play_body(p, env!("CARGO_PKG_VERSION"), &random_hex(16));
        let path = format!("/connect-state/v1/player/command/from/{from_device}/to/{to_device}");
        self.spclient_post(Lane::User, &path, &body).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, header, method, path},
    };

    use super::*;
    use crate::{SpotError, web::test_support::fixture};

    const CONTEXT: &str = "spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi";
    const TRACK: &str = "spotify:track:4SWyOAtHVNdnczEIsLvMN2";
    const UID: &str = "8a2f49e468f24758";

    fn play() -> ConnectPlay {
        ConnectPlay {
            context_uri: CONTEXT.into(),
            track_uri: Some(TRACK.into()),
            track_uid: Some(UID.into()),
            shuffle: true,
        }
    }

    fn is_hex(s: &str) -> bool {
        s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    #[test]
    fn connect_body_exact() {
        let id = "ab".repeat(16);
        assert_eq!(
            connect_play_body(&play(), "0.1.0", &id),
            json!({"command": {
                "endpoint": "play",
                "context": {"uri": CONTEXT, "url": format!("context://{CONTEXT}"), "metadata": {}},
                "play_origin": {"feature_identifier": "loudauto", "feature_version": "0.1.0"},
                "options": {
                    "license": "premium",
                    "skip_to": {"track_uri": TRACK, "track_uid": UID},
                    "player_options_override": {"shuffling_context": true},
                },
                "logging_params": {"command_id": id},
            }})
        );

        let no_uid = ConnectPlay {
            track_uid: None,
            ..play()
        };
        let body = connect_play_body(&no_uid, "0.1.0", &id);
        assert_eq!(
            body.pointer("/command/options/skip_to"),
            Some(&json!({"track_uri": TRACK})),
            "no track_uid key"
        );

        let from_the_top = ConnectPlay {
            track_uri: None,
            track_uid: None,
            shuffle: false,
            ..play()
        };
        let body = connect_play_body(&from_the_top, "0.1.0", &id);
        assert_eq!(body.pointer("/command/options/skip_to"), Some(&json!({})));
        assert_eq!(
            body.pointer("/command/options/player_options_override/shuffling_context"),
            Some(&json!(false))
        );
    }

    #[test]
    fn random_hex_lengths() {
        let from = random_hex(20);
        let command = random_hex(16);
        assert_eq!(from.len(), 40);
        assert_eq!(command.len(), 32);
        assert!(is_hex(&from) && is_hex(&command), "{from} {command}");
        assert_ne!(random_hex(16), random_hex(16));
        assert_eq!(random_hex(0), "");
    }

    #[tokio::test]
    async fn connect_posts_to_device_path() {
        let server = MockServer::start().await;
        let from = "f0".repeat(20);
        let to = "0123456789abcdef0123456789abcdef01234567";
        Mock::given(method("POST"))
            .and(path(format!(
                "/connect-state/v1/player/command/from/{from}/to/{to}"
            )))
            .and(header("authorization", "Bearer b1"))
            .and(header("client-token", "ct"))
            .and(body_partial_json(json!({"command": {
                "endpoint": "play",
                "context": {"uri": CONTEXT},
                "play_origin": {"feature_version": env!("CARGO_PKG_VERSION")},
            }})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ack_id": "a1"})))
            .expect(1)
            .mount(&server)
            .await;
        // A Background request outstanding for an hour: a User request must not wait.
        let f = fixture(&server, Duration::from_secs(3600)).await;
        assert!(f.lanes.admit(Lane::Background).await);

        tokio::time::timeout(
            Duration::from_secs(5),
            f.web.connect_play(&from, to, &play()),
        )
        .await
        .expect("sent on the User lane")
        .unwrap();
        server.verify().await;

        let requests = server.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let id = sent
            .pointer("/command/logging_params/command_id")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(id.len(), 32);
        assert!(is_hex(id), "{id}");
        assert_eq!(
            sent,
            connect_play_body(&play(), env!("CARGO_PKG_VERSION"), id)
        );
    }

    #[tokio::test]
    async fn connect_refusal_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let f = fixture(&server, Duration::ZERO).await;

        let err = f.web.connect_play("f", "d", &play()).await.unwrap_err();
        assert!(
            matches!(&err, SpotError::Http { status: 404, what } if what == "/connect-state/v1/player/command/from/f/to/d"),
            "{err:?}"
        );
    }
}
