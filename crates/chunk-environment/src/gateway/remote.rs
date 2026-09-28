//! A gateway machine: the gateway alone, joined to core on another machine over the environment's private network.

use super::{Gateway, GatewayConfig};
use crate::PlatformTarget;
use chunk_proto::sync::v1::{
    GatewayDeployment, SubscribeRequest, Update, core_client::CoreClient, entry::State, error::Code,
};
use chunk_proxy::GatewayCredential;
use prost::Message;
use std::{io, net::SocketAddr, sync::OnceLock, time::Duration};
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;

const RESUBSCRIBE: Duration = Duration::from_secs(1);

/// The core a gateway machine joins.
pub struct RemoteCore {
    /// Core's network endpoint, `http://<address>:<port>`, at a loopback or private address.
    pub endpoint: String,
    /// The machine credential core minted for this gateway.
    pub credential: String,
    /// This machine's environment, which the credential must name.
    pub environment: String,
}

impl RemoteCore {
    /// The gateway ID the credential names, once it names this environment. Core authorizes the credential by its own
    /// records, whatever ID it names.
    pub(crate) fn gateway(&self) -> io::Result<GatewayCredential> {
        let invalid = || io::Error::other("CHUNK_GATEWAY_CREDENTIAL is not a gateway machine credential");
        let (scope, _) = self.credential.rsplit_once('/').ok_or_else(invalid)?;
        let (scope, id) =
            scope.strip_prefix("machine/v1/").and_then(|scope| scope.rsplit_once('/')).ok_or_else(invalid)?;
        let environment = scope.strip_suffix("/gateway").filter(|_| !id.is_empty()).ok_or_else(invalid)?;
        if environment != self.environment {
            return Err(io::Error::other(format!(
                "the gateway credential belongs to environment {environment:?}, not {:?}",
                self.environment
            )));
        }
        Ok(GatewayCredential { id: id.to_owned(), credential: self.credential.clone() })
    }

    fn client(&self) -> io::Result<CoreClient<Channel>> {
        let address = self.endpoint.strip_prefix("http://").and_then(|address| address.parse::<SocketAddr>().ok());
        if !address.is_some_and(|address| chunk_service::net::private(address.ip())) {
            return Err(io::Error::other("CHUNK_CORE_ENDPOINT must be http://<address>:<port> at a private address"));
        }
        let channel = Channel::from_shared(self.endpoint.clone()).map_err(io::Error::other)?;
        Ok(CoreClient::new(channel.connect_timeout(Duration::from_secs(5)).connect_lazy()))
    }
}

/// Runs the gateway for `core` until `stop`, until core revokes its credential, or until the gateway stops on its own.
/// The listener starts once core's `deployment` topic first names a deployment, so no login is taken before then, and
/// each later one retargets it.
/// # Errors
/// Reports a credential of another environment, an endpoint that isn't private, a credential core rejects, a gateway
/// that stopped on its own, and gateway startup and shutdown errors.
pub(crate) async fn run(core: RemoteCore, config: GatewayConfig, stop: CancellationToken) -> io::Result<()> {
    let identity = core.gateway()?;
    let client = core.client()?;
    let gateway = OnceLock::new();
    let failed = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if gateway.get().is_some_and(Gateway::failed) {
                break;
            }
        }
    };
    let target = PlatformTarget { core: core.endpoint, gateway: identity, deployment: String::new() };
    tracing::info!(gateway = %target.gateway.id, core = %target.core, "joining core");
    let failure = tokio::select! {
        () = stop.cancelled() => Ok(()),
        () = failed => Err(io::Error::other("gateway stopped")),
        result = follow(client, target, &config, &gateway) => result,
    };
    let result = match gateway.into_inner() {
        Some(gateway) => gateway.stop().await.inspect_err(|error| tracing::error!(%error, "gateway shutdown failed")),
        None => Ok(()),
    };
    failure.and(result)
}

/// Follows core's `deployment` topic, subscribing again after interruptions, and routes the gateway to each deployment
/// it names. Returns once core revokes the credential.
async fn follow(
    mut client: CoreClient<Channel>,
    mut target: PlatformTarget,
    config: &GatewayConfig,
    gateway: &OnceLock<Gateway>,
) -> io::Result<()> {
    loop {
        let mut request = tonic::Request::new(SubscribeRequest { topic: "deployment".into(), ..Default::default() });
        let bearer = format!("Bearer {}", target.gateway.credential).parse().map_err(io::Error::other)?;
        request.metadata_mut().insert("authorization", bearer);
        let mut updates = match client.subscribe(request).await {
            Ok(updates) => updates.into_inner(),
            Err(status) if status.code() == tonic::Code::Unauthenticated => {
                return Err(io::Error::other("core rejected the gateway credential"));
            }
            Err(status) => {
                tracing::debug!(%status, "deployment topic unavailable");
                tokio::time::sleep(RESUBSCRIBE).await;
                continue;
            }
        };
        loop {
            let update = match updates.message().await {
                Ok(Some(update)) => update,
                Ok(None) => break,
                Err(status) => {
                    tracing::warn!(%status, "deployment topic interrupted");
                    break;
                }
            };
            if let Some(error) = &update.error {
                match error.code() {
                    Code::Stopped => {
                        tracing::warn!("core revoked the gateway credential; stopping");
                        return Ok(());
                    }
                    Code::Unavailable => {
                        tracing::warn!(message = %error.message, "deployment topic interrupted");
                        break;
                    }
                    _ => return Err(io::Error::other(format!("deployment topic failed: {}", error.message))),
                }
            }
            match deployment(&update)? {
                Some(deployment) if deployment != target.deployment => {
                    target.deployment = deployment;
                    tracing::info!(deployment = %target.deployment, "routing logins");
                    route(gateway, config, target.clone()).await?;
                }
                None if target.deployment.is_empty() => tracing::info!("waiting for a current release"),
                _ => {}
            }
        }
        tokio::time::sleep(RESUBSCRIBE).await;
    }
}

/// The deployment a snapshot of the `deployment` topic names, or `None` for an empty one or an update that isn't a
/// snapshot. The topic's one entry always fits in one update.
fn deployment(update: &Update) -> io::Result<Option<String>> {
    if !update.snapshot {
        return Ok(None);
    }
    let current = update.upserts.iter().find(|entry| entry.key == "current").and_then(|entry| entry.state.as_ref());
    let Some(State::Value(value)) = current else { return Ok(None) };
    let deployment = GatewayDeployment::decode(value.as_slice()).map_err(io::Error::other)?.deployment;
    Ok(Some(deployment).filter(|deployment| !deployment.is_empty()))
}

/// Sends later player connections to `target`, starting the gateway for the first one.
async fn route(gateway: &OnceLock<Gateway>, config: &GatewayConfig, target: PlatformTarget) -> io::Result<()> {
    if let Some(gateway) = gateway.get() {
        return gateway.retarget(target);
    }
    _ = gateway.set(Gateway::start(config.clone(), target).await?);
    Ok(())
}
