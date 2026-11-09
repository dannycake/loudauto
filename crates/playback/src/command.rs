//! Commands sent to Soloist: `{"type":"command","command":"<name>", ...fields}`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    Off,
    Context,
    Track,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    GetAuthState,
    GetState,
    GetQueue { limit: Option<u32> },
    Play { uri: Option<String> },
    Pause,
    SkipNext,
    SkipPrev,
    Seek { position_ms: u64 },
    SetVolume { volume: u8 },
    SetShuffle { enabled: bool },
    SetRepeatContext { enabled: bool },
    SetRepeatTrack { enabled: bool },
    AddToQueue { uri: String },
    Activate,
    Deactivate,
}

impl Command {
    pub fn to_json(&self) -> String {
        use Command as C;
        let (name, fields): (&str, Value) = match self {
            C::GetAuthState => ("get_auth_state", json!({})),
            C::GetState => ("get_state", json!({})),
            C::GetQueue { limit: Some(n) } => ("get_queue", json!({ "limit": n })),
            C::GetQueue { limit: None } => ("get_queue", json!({})),
            C::Play { uri: Some(uri) } => ("play", json!({ "uri": uri })),
            C::Play { uri: None } => ("play", json!({})),
            C::Pause => ("pause", json!({})),
            C::SkipNext => ("skip_next", json!({})),
            C::SkipPrev => ("skip_prev", json!({})),
            C::Seek { position_ms } => ("seek", json!({ "position_ms": position_ms })),
            C::SetVolume { volume } => ("set_volume", json!({ "volume": (*volume).min(100) })),
            C::SetShuffle { enabled } => ("set_shuffle", json!({ "enabled": enabled })),
            C::SetRepeatContext { enabled } => {
                ("set_repeat_context", json!({ "enabled": enabled }))
            }
            C::SetRepeatTrack { enabled } => ("set_repeat_track", json!({ "enabled": enabled })),
            C::AddToQueue { uri } => ("add_to_queue", json!({ "uri": uri })),
            C::Activate => ("activate", json!({})),
            C::Deactivate => ("deactivate", json!({})),
        };
        let mut msg = Map::new();
        msg.insert("type".into(), json!("command"));
        msg.insert("command".into(), json!(name));
        if let Value::Object(extra) = fields {
            msg.extend(extra);
        }
        Value::Object(msg).to_string()
    }
}

/// `SetVolume` clamped to Soloist's 0–100 range.
pub fn set_volume(volume: u8) -> Command {
    Command::SetVolume {
        volume: volume.min(100),
    }
}

/// Soloist has no single repeat-mode command; its docs prescribe these pairs, in order.
pub fn repeat_commands(mode: Repeat) -> [Command; 2] {
    use Command::{SetRepeatContext as Ctx, SetRepeatTrack as Trk};
    match mode {
        Repeat::Off => [Trk { enabled: false }, Ctx { enabled: false }],
        Repeat::Context => [Trk { enabled: false }, Ctx { enabled: true }],
        Repeat::Track => [Ctx { enabled: false }, Trk { enabled: true }],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn v(c: &Command) -> Value {
        serde_json::from_str(&c.to_json()).unwrap()
    }

    #[test]
    fn play_with_and_without_uri() {
        assert_eq!(
            v(&Command::Play { uri: None }),
            json!({"type":"command","command":"play"})
        );
        assert_eq!(
            v(&Command::Play {
                uri: Some("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".into())
            }),
            json!({"type":"command","command":"play","uri":"spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"})
        );
    }

    #[test]
    fn fields() {
        assert_eq!(
            v(&Command::Seek { position_ms: 30000 }),
            json!({"type":"command","command":"seek","position_ms":30000})
        );
        assert_eq!(
            v(&Command::SetShuffle { enabled: true }),
            json!({"type":"command","command":"set_shuffle","enabled":true})
        );
        assert_eq!(
            v(&Command::GetQueue { limit: Some(10) }),
            json!({"type":"command","command":"get_queue","limit":10})
        );
        assert_eq!(
            v(&Command::GetQueue { limit: None }),
            json!({"type":"command","command":"get_queue"})
        );
        assert_eq!(
            v(&Command::AddToQueue {
                uri: "spotify:track:6rqhFgbbKwnb9MLmUQDhG6".into()
            }),
            json!({"type":"command","command":"add_to_queue","uri":"spotify:track:6rqhFgbbKwnb9MLmUQDhG6"})
        );
    }

    #[test]
    fn volume_is_clamped() {
        assert_eq!(
            v(&set_volume(250)),
            json!({"type":"command","command":"set_volume","volume":100})
        );
        assert_eq!(
            v(&set_volume(42)),
            json!({"type":"command","command":"set_volume","volume":42})
        );
    }

    #[test]
    fn unit_command_names() {
        for (cmd, name) in [
            (Command::Pause, "pause"),
            (Command::SkipNext, "skip_next"),
            (Command::SkipPrev, "skip_prev"),
            (Command::GetAuthState, "get_auth_state"),
            (Command::GetState, "get_state"),
            (Command::Activate, "activate"),
            (Command::Deactivate, "deactivate"),
        ] {
            assert_eq!(v(&cmd), json!({"type":"command","command":name}));
        }
    }

    #[test]
    fn repeat_pairs_follow_docs() {
        use Command::{SetRepeatContext as Ctx, SetRepeatTrack as Trk};
        assert_eq!(
            repeat_commands(Repeat::Off),
            [Trk { enabled: false }, Ctx { enabled: false }]
        );
        assert_eq!(
            repeat_commands(Repeat::Context),
            [Trk { enabled: false }, Ctx { enabled: true }]
        );
        assert_eq!(
            repeat_commands(Repeat::Track),
            [Ctx { enabled: false }, Trk { enabled: true }]
        );
    }
}
