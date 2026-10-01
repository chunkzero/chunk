//! Wire shape of command effects exchanged between the backend and the proxy, and of why a move was refused.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Effect {
    Message {
        text: String,
    },
    ActionBar {
        text: String,
    },
    Title {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
    },
    Enter {
        destination: EffectDestination,
    },
    SessionCall {
        method: EffectMethod,
        arguments: Value,
    },
    SessionSend {
        method: EffectMethod,
        arguments: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectMethod {
    pub app: String,
    pub session: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectDestination {
    pub key: String,
    pub session_type: String,
    pub machine_profile: String,
}

/// Why a move of a player was refused, which queued nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveRefusal {
    /// The player holds no claim.
    Offline,
    /// The claim the move names is no longer the player's arrived claim, or the player is still arriving or already
    /// moving.
    Stale,
    /// The destination admits one session, and that session is full.
    Full,
    /// The player's release offers no such destination.
    UnknownDestination,
}
