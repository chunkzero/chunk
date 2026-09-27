//! The fake release, the fake player tests place on it, and a core whose JVM runs them.

use super::{Fixture, jvm::Launches, jvm_effects::SyncJvm};
use chunk_proto::{
    sync::v1::JvmDeliveryPhase,
    v1::{ClaimPhase, ClaimRequest, Identity, SessionDemand},
};
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::task::JoinHandle;

pub const PLAYER: &str = "00000000-0000-0000-0000-000000000001";

pub fn release() -> chunk_control::Release {
    serde_json::from_value(serde_json::json!({
        "apps": {"bridge": {"id": "bridge", "jar": "bridge.jar", "sha256": "digest", "java_version": 25,
            "sessions": {"default": {"machine_profile": "small", "capacity": 8}}}},
        "deployment": {"environment": "test", "deployment": "test"}, "artifact_digest": "digest",
        "profiles": {"small": {"memory_mib": 512, "max_sessions": 2}},
        "session_types": {"bridge/default": {"app": "bridge", "machine_profile": "small", "capacity": 8}},
        "max_processes": 1, "idle_node_timeout_seconds": 0, "session_methods": session_methods()
    }))
    .unwrap()
}

/// Declares `status`, which the fake JVM answers with 7, or with `limit` 0 holds queued until it's cancelled. Its
/// optional `pad` only adds size.
pub fn session_methods() -> serde_json::Value {
    serde_json::json!({"version": 1, "methods": [{
        "app": "bridge", "session": "default", "name": "status",
        "arguments": {"type": "object", "fields": {
            "limit": {"schema": {"type": "integer"}}, "pad": {"schema": {"type": "string"}, "optional": true}
        }},
        "result": {"type": "integer"}
    }]})
}

pub fn demand(key: &str) -> SessionDemand {
    SessionDemand { key: key.into(), session_type: "bridge/default".into(), machine_profile: "small".into() }
}

pub fn login() -> ClaimRequest {
    ClaimRequest {
        operation_id: "login".into(),
        proxy_id: "proxy".into(),
        connection_id: "connection".into(),
        identity: Some(Identity { uuid: PLAYER.into(), username: "player".into(), properties: vec![] }),
        demand: Some(demand("lobby")),
        source: None,
        deployment: String::new(),
    }
}

/// A fake JVM a test runs, until aborted.
pub struct Running {
    task: JoinHandle<()>,
    jvm: Arc<OnceLock<SyncJvm>>,
}

impl Running {
    pub fn abort(&self) {
        self.task.abort();
    }

    /// How many methods the JVM completed, held until cancelled, and cancelled.
    pub fn methods(&self) -> (usize, usize, usize) {
        self.jvm.get().map(SyncJvm::methods).unwrap_or_default()
    }
}

/// Starts core with a host whose JVM registers over sync once control launches it, and runs that JVM. The fake player
/// arrives once their claim activates, as when the proxy connects them.
pub async fn with_jvm() -> (Fixture, Running) {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    fixture.control.activate_release(release()).unwrap();
    let (client, control) = (fixture.client.clone(), fixture.control.clone());
    let jvm = Arc::new(OnceLock::new());
    let running = jvm.clone();
    let task = tokio::spawn(async move {
        let host = loop {
            if let Some(host) = launches.host() {
                break host;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let jvm = SyncJvm::connect(client, &host).await;
        let _ = running.set(jvm.clone());
        loop {
            let players = control.players().unwrap().players;
            if players.iter().any(|player| player.phase() == ClaimPhase::Activating) {
                for operation in jvm.prepared() {
                    jvm.player(&operation, JvmDeliveryPhase::Arrived).await;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    (fixture, Running { task, jvm })
}
