use serde::{Deserialize, Serialize};

/// Private connection records are exchanged only between trusted local processes.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConnection {
    pub endpoint: String,
    pub token: String,
    /// Native proxy lifecycle authority; never pass this credential to gameplay JVMs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_token: Option<String>,
    pub environment: String,
    pub deployment: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConnection {
    pub endpoint: String,
    pub token: String,
}
