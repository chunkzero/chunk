pub(super) struct PacketSpec {
    pub state: &'static str,
    pub direction: &'static str,
    pub source: &'static str,
    pub name: &'static str,
    pub fields: &'static [FieldSpec],
}

/// Limits supplement bounds absent from `ProtoDef`; paths include nested fields.
pub(super) struct FieldSpec {
    pub path: &'static str,
    pub rename: Option<&'static str>,
    pub limit: Option<usize>,
}

const fn bounded(path: &'static str, limit: usize) -> FieldSpec {
    FieldSpec {
        path,
        rename: None,
        limit: Some(limit),
    }
}

const fn renamed(path: &'static str, rename: &'static str, limit: Option<usize>) -> FieldSpec {
    FieldSpec {
        path,
        rename: Some(rename),
        limit,
    }
}

macro_rules! packets {
    ($(($state:literal, $direction:literal, $source:literal, $name:literal, [$($field:expr),* $(,)?])),* $(,)?) => {
        pub(super) const PACKETS: &[PacketSpec] = &[$(PacketSpec {
            state: $state, direction: $direction, source: $source, name: $name, fields: &[$($field),*],
        }),*];
    };
}

// Collection limits are local resource bounds, not additional version support.
packets![
    (
        "handshaking",
        "toServer",
        "set_protocol",
        "Handshake",
        [renamed("serverHost", "server_address", Some(255))]
    ),
    ("status", "toServer", "ping_start", "StatusRequest", []),
    (
        "status",
        "toClient",
        "server_info",
        "StatusResponse",
        [renamed("response", "json", Some(32767))]
    ),
    ("status", "toServer", "ping", "Ping", [renamed("time", "payload", None)]),
    ("status", "toClient", "ping", "Pong", [renamed("time", "payload", None)]),
    (
        "login",
        "toClient",
        "disconnect",
        "LoginDisconnect",
        [bounded("reason", 32767)]
    ),
    (
        "login",
        "toServer",
        "login_start",
        "LoginStart",
        [bounded("username", 16), renamed("playerUUID", "player_uuid", None)]
    ),
    (
        "login",
        "toClient",
        "encryption_begin",
        "EncryptionRequest",
        [
            bounded("serverId", 20),
            bounded("publicKey", 1_048_576),
            bounded("verifyToken", 1_048_576),
        ]
    ),
    (
        "login",
        "toServer",
        "encryption_begin",
        "EncryptionResponse",
        [bounded("sharedSecret", 1_048_576), bounded("verifyToken", 1_048_576)]
    ),
    (
        "login",
        "toClient",
        "success",
        "LoginSuccess",
        [
            bounded("username", 16),
            bounded("properties", 1024),
            bounded("properties.name", 32767),
            bounded("properties.value", 32767),
            bounded("properties.signature", 32767),
        ]
    ),
    ("login", "toClient", "compress", "SetCompression", []),
    ("login", "toServer", "login_acknowledged", "LoginAcknowledged", []),
    (
        "login",
        "toClient",
        "login_plugin_request",
        "LoginPluginRequest",
        [bounded("channel", 32767), bounded("data", 1_048_576)]
    ),
    (
        "login",
        "toServer",
        "login_plugin_response",
        "LoginPluginResponse",
        [bounded("data", 1_048_576)]
    ),
    (
        "configuration",
        "toServer",
        "settings",
        "ConfigurationClientInformation",
        [bounded("locale", 16)]
    ),
    ("configuration", "toClient", "keep_alive", "ConfigurationKeepAlive", []),
    (
        "configuration",
        "toServer",
        "keep_alive",
        "ConfigurationKeepAliveResponse",
        []
    ),
    ("configuration", "toClient", "ping", "ConfigurationPing", []),
    ("configuration", "toServer", "pong", "ConfigurationPong", []),
    (
        "configuration",
        "toClient",
        "custom_payload",
        "ConfigurationPluginMessage",
        [bounded("channel", 32767), bounded("data", 1_048_576)]
    ),
    (
        "configuration",
        "toServer",
        "custom_payload",
        "ConfigurationPluginResponse",
        [bounded("channel", 32767), bounded("data", 32767)]
    ),
    (
        "configuration",
        "toClient",
        "finish_configuration",
        "FinishConfiguration",
        []
    ),
    (
        "configuration",
        "toServer",
        "finish_configuration",
        "AcknowledgeConfiguration",
        []
    ),
    (
        "configuration",
        "toClient",
        "select_known_packs",
        "SelectKnownPacks",
        [
            bounded("packs", 1024),
            bounded("packs.namespace", 32767),
            bounded("packs.id", 32767),
            bounded("packs.version", 32767),
        ]
    ),
    (
        "configuration",
        "toServer",
        "select_known_packs",
        "KnownPacks",
        [
            bounded("packs", 1024),
            bounded("packs.namespace", 32767),
            bounded("packs.id", 32767),
            bounded("packs.version", 32767),
        ]
    ),
    (
        "configuration",
        "toClient",
        "feature_flags",
        "FeatureFlags",
        [bounded("features", 1024), bounded("features[]", 32767)]
    ),
];
