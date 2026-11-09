//! Soloist's WebSocket messages as they arrive. Only the fields LOUDAUTO uses are
//! modelled; everything else is ignored so new fields never break parsing.
//!
//! Parsing is lenient: an optional field that is ill-typed is treated as missing (and a bad
//! element of a list is skipped), so one surprise never costs the rest of a frame. Only the
//! fields a value can't be used without are required: a uri, a name, the payload of a
//! single-purpose frame (`track_changed`'s item, `volume_changed`'s volume, …). A frame that
//! lacks one is dropped and logged at `warn` with its `type`.

use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::{Map, Value};

/// An optional field: `None` when it is missing or ill-typed.
fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    Ok(serde_json::from_value(Value::deserialize(d)?).ok())
}

/// A field with a default: the default when it is missing or ill-typed.
fn lenient_or_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    lenient(d).map(Option::unwrap_or_default)
}

/// A list: ill-typed elements are skipped; anything but an array is empty.
fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    Ok(match Value::deserialize(d)? {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect(),
        _ => Vec::new(),
    })
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Entity {
    pub uri: String,
    #[serde(default, deserialize_with = "lenient")]
    pub entity_type: Option<String>,
    #[serde(default, deserialize_with = "lenient_or_default")]
    pub decorations: Decorations,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct Decorations {
    #[serde(deserialize_with = "lenient")]
    pub identity: Option<Identity>,
    #[serde(deserialize_with = "lenient")]
    pub visual_identity: Option<VisualIdentity>,
    #[serde(deserialize_with = "lenient")]
    pub parent: Option<Box<EntityRef>>,
    #[serde(deserialize_with = "lenient_vec")]
    pub creators: Vec<EntityRef>,
    #[serde(deserialize_with = "lenient")]
    pub playback: Option<PlaybackInfo>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Identity {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct VisualIdentity {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub cover: Vec<Cover>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Cover {
    pub url: String,
    pub size: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EntityRef {
    pub entity: Entity,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PlaybackInfo {
    #[serde(default, deserialize_with = "lenient")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WirePosition {
    pub position_ms: u64,
    pub timestamp_ms: i64,
    pub speed: f64,
}

/// Shuffle and repeat as Soloist reports them; `repeat` is `"off"`, `"context"` or `"track"`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WireOptions {
    pub shuffle: bool,
    pub repeat: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct QueueEntry {
    #[serde(default, deserialize_with = "lenient")]
    pub uid: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub source: Option<String>,
    pub item: Entity,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    AuthState {
        logged_in: bool,
        #[serde(default, deserialize_with = "lenient_or_default")]
        is_active: bool,
    },
    /// The whole state. Every field is optional; each one present becomes an event.
    PlaybackState {
        #[serde(default, deserialize_with = "lenient")]
        status: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        item: Option<Entity>,
        #[serde(default, deserialize_with = "lenient")]
        position: Option<WirePosition>,
        #[serde(default, deserialize_with = "lenient")]
        volume: Option<u8>,
        #[serde(default, deserialize_with = "lenient_or_default")]
        is_active: bool,
        /// Boxed so a second inline `Entity` doesn't make every `Incoming` this large.
        #[serde(default, deserialize_with = "lenient")]
        context: Option<Box<Entity>>,
        #[serde(default, deserialize_with = "lenient")]
        options: Option<WireOptions>,
        /// One key per action allowed now; the values (e.g. `seek_forward`'s `step_ms`) are unused.
        #[serde(default, deserialize_with = "lenient")]
        available_actions: Option<Map<String, Value>>,
    },
    TrackChanged {
        item: Entity,
    },
    PlaybackChanged {
        status: String,
    },
    VolumeChanged {
        volume: u8,
    },
    DeviceChanged {
        is_active: bool,
    },
    PositionSync {
        position: WirePosition,
    },
    /// `context` is null or missing when nothing is playing from a context.
    ContextChanged {
        #[serde(default)]
        context: Option<Entity>,
    },
    OptionsChanged {
        options: WireOptions,
    },
    QueueChanged {
        #[serde(default, deserialize_with = "lenient_vec")]
        upcoming: Vec<QueueEntry>,
    },
    CommandResult {
        command: String,
    },
    Error {
        message: String,
    },
    #[serde(other)]
    Unknown,
}

/// `None`, logged at `warn`, for a frame that can't be used: not JSON, no `type`, or a known
/// type without a field it needs. Unknown types are `Some(Incoming::Unknown)`.
pub fn parse_incoming(text: &str) -> Option<Incoming> {
    let err = match serde_json::from_str(text) {
        Ok(incoming) => return Some(incoming),
        Err(err) => err,
    };
    let frame: Option<Value> = serde_json::from_str(text).ok();
    match frame.as_ref().and_then(|f| f.get("type")?.as_str()) {
        Some(kind) => log::warn!("dropped a Soloist {kind} frame: {err}"),
        None => log::warn!("dropped a Soloist frame without a type: {err}"),
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURE: &str = include_str!("../tests/fixtures/state.json");

    fn frames() -> Vec<Option<Incoming>> {
        let raw: Vec<serde_json::Value> = serde_json::from_str(CAPTURE).unwrap();
        raw.iter().map(|v| parse_incoming(&v.to_string())).collect()
    }

    #[test]
    fn parses_live_capture() {
        let frames = frames();
        assert_eq!(
            frames[0],
            Some(Incoming::AuthState {
                logged_in: true,
                is_active: false
            })
        );
        let Some(Incoming::PlaybackState {
            status,
            item: Some(item),
            volume,
            is_active,
            context: Some(context),
            options: Some(options),
            available_actions: Some(actions),
            ..
        }) = &frames[1]
        else {
            panic!("second frame: {:?}", frames[1]);
        };
        assert_eq!(status.as_deref(), Some("playing"));
        assert!(!is_active);
        assert_eq!(*volume, Some(100));
        assert_eq!(context.uri, "spotify:playlist:37i9dQZF1F5p3rmiWPIYgZ");
        assert_eq!(
            *options,
            WireOptions {
                shuffle: false,
                repeat: "off".into()
            }
        );
        assert!(actions.contains_key("pause") && !actions.contains_key("play"));
        assert_eq!(item.uri, "spotify:track:1XD4K4CGAKTIBmFpvuaFru");
        let d = &item.decorations;
        assert_eq!(
            d.identity.as_ref().unwrap().name,
            "Dancing with your eyes closed"
        );
        let parent = d.parent.as_ref().unwrap();
        assert_eq!(
            parent.entity.decorations.identity.as_ref().unwrap().name,
            "Revengeseekerz"
        );
        assert_eq!(d.creators.len(), 1);
        assert_eq!(
            d.creators[0]
                .entity
                .decorations
                .identity
                .as_ref()
                .unwrap()
                .name,
            "Jane Remover"
        );
        assert_eq!(d.playback.as_ref().unwrap().duration_ms, Some(230526));
        assert_eq!(d.visual_identity.as_ref().unwrap().cover.len(), 4);
        assert!(frames.iter().any(
            |f| matches!(f, Some(Incoming::QueueChanged { upcoming }) if upcoming.len() == 3)
        ));
    }

    #[test]
    fn unknown_type_is_unknown() {
        let f = parse_incoming(r#"{"type":"something_new","context":{"uri":"spotify:album:x"}}"#);
        assert_eq!(f, Some(Incoming::Unknown));
    }

    #[test]
    fn context_and_options_frames() {
        assert_eq!(
            parse_incoming(r#"{"type":"context_changed","context":{"uri":"spotify:album:x"}}"#),
            Some(Incoming::ContextChanged {
                context: Some(Entity {
                    uri: "spotify:album:x".into(),
                    entity_type: None,
                    decorations: Decorations::default(),
                })
            })
        );
        assert_eq!(
            parse_incoming(
                r#"{"type":"options_changed","options":{"shuffle":true,"repeat":"context","playback_speed":1}}"#
            ),
            Some(Incoming::OptionsChanged {
                options: WireOptions {
                    shuffle: true,
                    repeat: "context".into()
                }
            })
        );
    }

    #[test]
    fn unusable_frames_are_dropped() {
        for text in [
            r#"{"type":"track_changed"}"#,
            r#"{"type":"track_changed","item":{"decorations":{}}}"#,
            r#"{"type":"volume_changed","volume":"loud"}"#,
            r#"{"type":"volume_changed","volume":300}"#,
            r#"{"type":"options_changed","options":{}}"#,
            r#"{"type":"context_changed","context":5}"#,
            r#"{"type":"auth_state","is_active":true}"#,
            r#"{"type":7}"#,
            r#"{"volume":42}"#,
            r#"[1,2]"#,
        ] {
            assert_eq!(parse_incoming(text), None, "{text}");
        }
    }

    #[test]
    fn error_frame() {
        let f = parse_incoming(r#"{"type":"error","message":"command requires authentication"}"#);
        assert_eq!(
            f,
            Some(Incoming::Error {
                message: "command requires authentication".into()
            })
        );
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(parse_incoming("{not json"), None);
    }

    #[test]
    fn doc_examples() {
        assert_eq!(
            parse_incoming(
                r#"{"type":"position_sync","position":{"position_ms":45000,"timestamp_ms":1747654321000,"speed":1.0}}"#
            ),
            Some(Incoming::PositionSync {
                position: WirePosition {
                    position_ms: 45000,
                    timestamp_ms: 1747654321000,
                    speed: 1.0
                }
            })
        );
        assert_eq!(
            parse_incoming(r#"{"type":"volume_changed","volume":42}"#),
            Some(Incoming::VolumeChanged { volume: 42 })
        );
        assert_eq!(
            parse_incoming(
                r#"{"type":"device_changed","is_active":true,"device_name":"Kitchen speaker"}"#
            ),
            Some(Incoming::DeviceChanged { is_active: true })
        );
        assert_eq!(
            parse_incoming(r#"{"type":"command_result","command":"pause"}"#),
            Some(Incoming::CommandResult {
                command: "pause".into()
            })
        );
    }
}
