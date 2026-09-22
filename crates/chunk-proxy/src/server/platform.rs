use std::{io, sync::Arc, time::Duration};

use chunk_proto::v1::{
    BackendQuery, SessionDemand, backend_client::BackendClient, backend_commands_client::BackendCommandsClient,
    local_control_client::LocalControlClient,
};
use chunk_protocol::{
    McString, encode_packet,
    versions::{SUPPORTED, v26_2::StatusResponse},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tonic::{Request, transport::Channel};

use super::transport::invalid_data;
use crate::PlatformTarget;

mod native;
pub(in crate::server) use native::Lifecycle;

pub(super) const RPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct Platform {
    pub target: PlatformTarget,
    pub cleanup: tokio_util::task::TaskTracker,
    pub proxy_id: String,
    pub control: LocalControlClient<Channel>,
    pub commands: BackendCommandsClient<Channel>,
    backend: BackendClient<Channel>,
    hooks: Arc<Semaphore>,
    status_hooks: Arc<Semaphore>,
    native: native::Native,
}

impl Platform {
    pub fn new(target: PlatformTarget) -> io::Result<Self> {
        Ok(Self {
            control: LocalControlClient::new(channel(&target.control.endpoint)?)
                .max_decoding_message_size(8 * 1024 * 1024),
            commands: BackendCommandsClient::new(channel(&target.backend.endpoint)?)
                .max_decoding_message_size(1024 * 1024),
            backend: BackendClient::new(channel(&target.backend.endpoint)?),
            native: native::Native::new(&target.backend.endpoint)?,
            target,
            cleanup: tokio_util::task::TaskTracker::new(),
            proxy_id: uuid::Uuid::new_v4().to_string(),
            hooks: Arc::new(Semaphore::new(64)),
            status_hooks: Arc::new(Semaphore::new(64)),
        })
    }

    pub fn control_request<T>(&self, body: T) -> io::Result<Request<T>> {
        request(body, &self.target.control.token)
    }

    /// Authenticates `body` with `token` for this platform's backend environment and deployment.
    pub fn backend_request<T>(&self, body: T, token: &str) -> io::Result<Request<T>> {
        let backend = &self.target.backend;
        let mut request = request(body, token)?;
        request.metadata_mut().insert("x-chunk-environment", backend.environment.parse().map_err(invalid_data)?);
        request.metadata_mut().insert("x-chunk-deployment", backend.deployment.parse().map_err(invalid_data)?);
        Ok(request)
    }

    async fn hook<T: DeserializeOwned>(&self, phase: &str, arguments: Value) -> io::Result<T> {
        let hooks = if phase == "status" { &self.status_hooks } else { &self.hooks };
        let _permit = hooks.try_acquire().map_err(|_| io::Error::other("backend hook capacity exhausted"))?;
        let invocation = self.backend_request(
            BackendQuery {
                function: format!("shared/proxy/{phase}"),
                arguments_json: serde_json::to_vec(&arguments).map_err(invalid_data)?,
                caller_json: serde_json::to_vec(&json!({"kind": "proxy", "phase": phase, "proxyId": self.proxy_id}))
                    .map_err(invalid_data)?,
            },
            &self.target.backend.token,
        )?;
        let result = self.backend.clone().query(invocation).await.map_err(io::Error::other)?.into_inner();
        serde_json::from_slice(&result.result_json).map_err(invalid_data)
    }

    async fn legacy_route(&self, uuid: &str, username: &str) -> io::Result<SessionDemand> {
        let arguments = json!({"uuid": uuid, "username": username});
        self.admit(arguments.clone()).await?;
        let route: Route = self.hook("route", arguments).await?;
        Ok(SessionDemand { key: route.key, session_type: route.session_type, machine_profile: route.machine_profile })
    }

    async fn admit(&self, arguments: Value) -> io::Result<()> {
        let admission: Admission = self.hook("admit", arguments.clone()).await?;
        if !admission.allow {
            let reason = admission.reason.unwrap_or_else(|| "Admission denied.".into());
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason.chars().take(256).collect::<String>()));
        }
        Ok(())
    }

    async fn legacy_approve_move(&self, claim: &chunk_proto::v1::ClaimRequest) -> io::Result<()> {
        let identity = claim.identity.as_ref().ok_or_else(|| invalid_data("missing move identity"))?;
        let demand = claim.demand.as_ref().ok_or_else(|| invalid_data("missing move demand"))?;
        self.admit(json!({"uuid": identity.uuid, "username": identity.username})).await?;
        let route: Route = self.hook("move", json!({
            "uuid": identity.uuid, "username": identity.username,
            "destination": {"key": demand.key, "session_type": demand.session_type, "machine_profile": demand.machine_profile}
        })).await?;
        if route.key != demand.key
            || route.session_type != demand.session_type
            || route.machine_profile != demand.machine_profile
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Move destination denied"));
        }
        Ok(())
    }

    pub async fn status(&self, host: &str) -> io::Result<Vec<u8>> {
        let result = match self.manifest().await {
            Ok(Some(manifest)) => self.native_status(&manifest, host).await,
            Ok(None) => self.hook("status", json!({"host": host})).await,
            Err(error) => Err(error),
        };
        let status: Status = result.unwrap_or_else(|error| {
            tracing::debug!(%error, "status hook unavailable");
            Status { motd: "Server temporarily unavailable".into(), online: 0, max: 0 }
        });
        let version = SUPPORTED.last().ok_or_else(|| invalid_data("missing protocol"))?;
        encode_packet(&StatusResponse {
            json: McString::new(
                json!({
                    "version": {"name": version.name, "protocol": version.protocol},
                    "players": {"online": status.online, "max": status.max},
                    "description": {"text": status.motd},
                })
                .to_string(),
            )
            .map_err(invalid_data)?,
        })
        .map_err(invalid_data)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    allow: bool,
    reason: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    key: String,
    session_type: String,
    machine_profile: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    motd: String,
    online: u32,
    max: u32,
}

pub(super) fn request<T>(body: T, token: &str) -> io::Result<Request<T>> {
    let mut request = Request::new(body);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().map_err(invalid_data)?);
    request.set_timeout(RPC_TIMEOUT);
    Ok(request)
}

fn channel(endpoint: &str) -> io::Result<Channel> {
    let address = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| invalid_data("local service requires http loopback endpoint"))?;
    let address: std::net::SocketAddr = address.parse().map_err(invalid_data)?;
    if !address.ip().is_loopback() {
        return Err(invalid_data("local service requires loopback endpoint"));
    }
    Ok(Channel::from_shared(endpoint.to_owned())
        .map_err(invalid_data)?
        .connect_timeout(RPC_TIMEOUT)
        .timeout(Duration::from_secs(45))
        .connect_lazy())
}

#[cfg(test)]
mod tests;
