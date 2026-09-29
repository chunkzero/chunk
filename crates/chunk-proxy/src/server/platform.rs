use std::{io, sync::Arc, time::Duration};

use chunk_contract::DomainManifest;
use chunk_proto::sync::v1::{ActiveArguments, CallRequest, Caller, Position, PrepareResult, SessionDemand};
use prost::Message;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::{OnceCell, Semaphore};
use tonic::{
    Request,
    transport::{Channel, Endpoint},
};

use super::{claim::Claim, transport::invalid_data};
use crate::{PlatformTarget, Reports};

mod claims;
mod commands;
mod native;
mod sync;
pub(in crate::server) use claims::{View, generation};
pub(in crate::server) use commands::CommandUpdate;
pub(in crate::server) use native::Lifecycle;
pub(in crate::server) use sync::failure;

pub(super) const RPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct Platform {
    pub target: PlatformTarget,
    pub cleanup: tokio_util::task::TaskTracker,
    hooks: Arc<Semaphore>,
    status_hooks: Arc<Semaphore>,
    /// The domain manifest of the target's deployment, read once.
    manifest: Arc<OnceCell<Option<Arc<DomainManifest>>>>,
    sync: Arc<sync::Connection>,
    /// Where each status answered is kept for core to report.
    pub reports: Arc<Reports>,
}

impl Platform {
    pub fn new(target: PlatformTarget) -> io::Result<Self> {
        let sync = sync::Connection::new(&target.core, target.gateway.clone())?;
        Ok(Self::with(target, Arc::new(sync), tokio_util::task::TaskTracker::new(), Arc::default()))
    }

    /// A platform for `target` that keeps this one's core client, claims follower, cleanup tracking and reports, and
    /// with them its core endpoint and gateway identity.
    pub fn retarget(&self, target: PlatformTarget) -> Self {
        Self::with(target, self.sync.clone(), self.cleanup.clone(), self.reports.clone())
    }

    fn with(
        target: PlatformTarget,
        sync: Arc<sync::Connection>,
        cleanup: tokio_util::task::TaskTracker,
        reports: Arc<Reports>,
    ) -> Self {
        Self {
            manifest: Arc::default(),
            cleanup,
            hooks: Arc::new(Semaphore::new(64)),
            status_hooks: Arc::new(Semaphore::new(64)),
            sync,
            reports,
            target,
        }
    }

    /// Waits for a live view of this gateway's claims in which `ready` returns a value.
    pub async fn claims<T>(&self, ready: impl FnMut(&View) -> Option<T>) -> io::Result<T> {
        self.sync.claims(ready).await
    }

    /// A new ID for a player's connection to this process. The claims of connections it names are never inherited.
    pub fn connection_id(&self) -> String {
        self.sync.connection_id()
    }

    /// Calls platform method `chunk:<method>` on the claim `operation` names, returning its result and control's
    /// position after it.
    pub async fn call<R: Message + Default>(
        &self,
        method: &str,
        operation: &str,
        arguments: &impl Message,
        timeout: Duration,
    ) -> io::Result<(R, Option<Position>)> {
        let message = CallRequest {
            operation_id: operation.to_owned(),
            method: format!("chunk:{method}"),
            arguments: arguments.encode_to_vec(),
            ..CallRequest::default()
        };
        self.fenced(message, timeout).await
    }

    /// Runs `message` on the gateway's current stream, decoding its result.
    async fn fenced<R: Message + Default>(
        &self,
        message: CallRequest,
        timeout: Duration,
    ) -> io::Result<(R, Option<Position>)> {
        let (result, position) = self.sync.fenced(message, timeout).await?;
        Ok((R::decode(result.as_slice()).map_err(invalid_data)?, position))
    }

    /// Calls app function or hook `method` of the target's deployment with JSON `arguments` under `operation`, which
    /// is empty for a query. The caller names `player` while this gateway holds their claim.
    async fn call_app(
        &self,
        operation: String,
        method: &str,
        arguments: &Value,
        player: Option<&str>,
    ) -> io::Result<Value> {
        let message = CallRequest {
            operation_id: operation,
            method: method.to_owned(),
            arguments: serde_json::to_vec(arguments).map_err(invalid_data)?,
            deployment: self.target.deployment.clone(),
            caller: player.map(|player| Caller { player: player.to_owned(), ..Caller::default() }),
            ..CallRequest::default()
        };
        serde_json::from_slice(&self.sync.unfenced(message).await?).map_err(invalid_data)
    }

    /// An operation ID for one effectful call.
    async fn prepare(&self) -> io::Result<String> {
        let message = CallRequest { method: "chunk:prepare".into(), ..CallRequest::default() };
        let result = self.sync.unfenced(message).await?;
        Ok(PrepareResult::decode(result.as_slice()).map_err(invalid_data)?.operation_id)
    }

    /// Tells core, on the gateway's current stream, how many connections this gateway holds.
    pub async fn active(&self, connections: u32) -> io::Result<()> {
        let message = CallRequest {
            method: "chunk:active".into(),
            arguments: ActiveArguments { connections }.encode_to_vec(),
            ..CallRequest::default()
        };
        self.sync.fenced(message, RPC_TIMEOUT).await.map(drop)
    }

    /// Queries the app's legacy `shared/proxy/<phase>` hook.
    async fn hook<T: DeserializeOwned>(&self, phase: &str, arguments: &Value, player: Option<&str>) -> io::Result<T> {
        let hooks = if phase == "status" { &self.status_hooks } else { &self.hooks };
        let _permit = hooks.try_acquire().map_err(|_| io::Error::other("backend hook capacity exhausted"))?;
        let result = self.call_app(String::new(), &format!("shared/proxy/{phase}"), arguments, player).await?;
        serde_json::from_value(result).map_err(invalid_data)
    }

    async fn legacy_route(&self, uuid: &str, username: &str) -> io::Result<SessionDemand> {
        let arguments = json!({"uuid": uuid, "username": username});
        self.admit(&arguments, None).await?;
        let route: Route = self.hook("route", &arguments, None).await?;
        Ok(SessionDemand { key: route.key, session_type: route.session_type, machine_profile: route.machine_profile })
    }

    async fn admit(&self, arguments: &Value, player: Option<&str>) -> io::Result<()> {
        self.hook::<Admission>("admit", arguments, player).await?.check()
    }

    async fn legacy_approve_move(&self, claim: &Claim) -> io::Result<()> {
        let (identity, demand) = (&claim.player, &claim.demand);
        let player = Some(identity.uuid.as_str());
        self.admit(&json!({"uuid": identity.uuid, "username": identity.username}), player).await?;
        let route: Route = self.hook("move", &json!({
            "uuid": identity.uuid, "username": identity.username,
            "destination": {"key": demand.key, "session_type": demand.session_type, "machine_profile": demand.machine_profile}
        }), player).await?;
        if route.key != demand.key
            || route.session_type != demand.session_type
            || route.machine_profile != demand.machine_profile
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Move destination denied"));
        }
        Ok(())
    }

    /// The status packet for `host`, from the app's hooks. Each one they answer is kept for core to report.
    pub async fn status(&self, host: &str) -> io::Result<Vec<u8>> {
        let result = match self.manifest().await {
            Ok(Some(manifest)) => self.native_status(&manifest, host).await,
            Ok(None) => self.hook("status", &json!({"host": host}), None).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(Status { motd, online, max }) => {
                let status = super::status_json(&motd, online, max)?;
                self.reports.ping(host, &status);
                super::encode_status(status)
            }
            Err(error) => {
                tracing::debug!(%error, "status hook unavailable");
                super::encode_status(super::status_json("Server temporarily unavailable", 0, 0)?)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    allow: bool,
    reason: Option<String>,
}

impl Admission {
    /// Denials carry at most 256 characters of the application's reason.
    fn check(self) -> io::Result<()> {
        if self.allow {
            return Ok(());
        }
        let reason = self.reason.unwrap_or_else(|| "Admission denied.".into());
        Err(io::Error::new(io::ErrorKind::PermissionDenied, reason.chars().take(256).collect::<String>()))
    }
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

fn authorized<T>(body: T, token: &str) -> io::Result<Request<T>> {
    let mut request = Request::new(body);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().map_err(invalid_data)?);
    Ok(request)
}

fn endpoint(endpoint: &str) -> io::Result<Endpoint> {
    let address =
        endpoint.strip_prefix("http://").ok_or_else(|| invalid_data("core endpoint must be an http address"))?;
    let address: std::net::SocketAddr = address.parse().map_err(invalid_data)?;
    if !chunk_service::net::private(address.ip()) {
        return Err(invalid_data("core endpoint must be a private address"));
    }
    Ok(Channel::from_shared(endpoint.to_owned())
        .map_err(invalid_data)?
        .connect_timeout(RPC_TIMEOUT)
        .timeout(Duration::from_secs(45)))
}

#[cfg(test)]
mod tests;
