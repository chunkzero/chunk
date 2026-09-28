use serde::{Deserialize, Serialize};

/// Private connection records are exchanged only between trusted local processes.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConnection {
    pub endpoint: String,
    pub token: String,
}
