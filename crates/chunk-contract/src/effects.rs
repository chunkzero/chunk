//! Wire shape of command effects exchanged between the backend and the proxy.

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
