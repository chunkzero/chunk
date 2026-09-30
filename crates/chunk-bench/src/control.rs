//! Control workloads: core's in-process gateway admitting and releasing synthetic players over the sync protocol, as a
//! proxy does, against the synthetic JVMs in `fixtures.rs`.
use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use chunk_proto::sync::v1::{
    ActivateResult, CallRequest, ClaimArguments, ClaimPhase, ClaimResult, DepartResult, GatewayArguments, GatewayClaim,
    GatewayLogin, PlayerIdentity, SessionDemand, SubscribeRequest, Update, claim_result, core_client::CoreClient,
    entry::State,
};
use prost::Message;
use tokio::sync::{Mutex, watch};
use tokio_util::task::AbortOnDropHandle;
use tonic::{Streaming, transport::Channel};

use crate::{
    config::{self, Config, Scenario},
    sync,
};

/// Each open claim's phase, by operation ID.
type Claims = BTreeMap<String, ClaimPhase>;

pub fn release() -> Result<chunk_control::Release> {
    Ok(serde_json::from_value(serde_json::json!({
        "apps": {"bench": {"id":"bench", "jar":"bench.jar", "sha256":"bench", "java_version":25,
            "sessions":{"default":{"machine_profile":"bench","capacity":128}}}},
        "deployment":{"environment":"bench","deployment":"bench"}, "release_id":"bench",
        "profiles":{"bench":{"memory_mib":512,"max_sessions":1}},
        "session_types":{"bench/default":{"app":"bench","machine_profile":"bench","capacity":128}},
        "max_processes": config::CONTROL_SLOTS / 128, "idle_node_timeout_seconds":0
    }))?)
}

/// Player `index`'s login, under the operation ID it returns.
fn login(index: u64) -> (String, ClaimArguments) {
    let login = GatewayLogin {
        connection_id: format!("connection-{index:08}"),
        player: Some(PlayerIdentity {
            uuid: uuid::Uuid::from_u128(u128::from(index) + 1).to_string(),
            username: "bench".into(),
            properties: vec![],
        }),
        demand: Some(SessionDemand {
            key: "lobby".into(),
            session_type: "bench/default".into(),
            machine_profile: "bench".into(),
        }),
        deployment: String::new(),
    };
    (format!("bench-{index:08}"), ClaimArguments { login: Some(login) })
}

/// Subscribes to the gateway's topic, returning the stream, its ID and its snapshot.
async fn subscribe(
    rpc: &mut CoreClient<Channel>,
    connection: &sync::Connection,
) -> Result<(Streaming<Update>, String, Claims)> {
    // Every subscription names one instance, so the bench's own subscriptions never take the topic from each other.
    let arguments = GatewayArguments { instance: "bench".into() }.encode_to_vec();
    let topic = format!("gateway/{}", connection.gateway_id);
    let subscription = SubscribeRequest { topic, arguments, ..SubscribeRequest::default() };
    let mut updates = rpc.subscribe(sync::request(subscription, &connection.gateway)?).await?.into_inner();
    let mut claims = Claims::new();
    let mut stream = String::new();
    loop {
        let update = updates.message().await?.context("gateway topic closed before its snapshot")?;
        if !update.stream.is_empty() {
            stream.clone_from(&update.stream);
        }
        let continued = update.continued;
        apply(&mut claims, update)?;
        if !continued {
            return Ok((updates, stream, claims));
        }
    }
}

fn apply(claims: &mut Claims, update: Update) -> Result<()> {
    if let Some(error) = update.error {
        bail!("gateway topic ended: {:?}: {}", error.code(), error.message);
    }
    if update.snapshot {
        claims.clear();
    }
    for operation in &update.removed {
        claims.remove(operation);
    }
    for entry in update.upserts {
        let Some(State::Value(value)) = entry.state else { bail!("gateway claim entry without a value") };
        claims.insert(entry.key, GatewayClaim::decode(&value[..])?.phase());
    }
    Ok(())
}

/// Checks that a fresh snapshot holds exactly `expected` claims, all arrived. It supersedes any earlier stream.
pub async fn verify_population(connection: &sync::Connection, expected: u32) -> Result<()> {
    let (_, _, claims) = subscribe(&mut sync::connect(&connection.endpoint).await?, connection).await?;
    ensure!(claims.len() == expected as usize, "{} claims instead of the population of {expected}", claims.len());
    ensure!(claims.values().all(|phase| *phase == ClaimPhase::Arrived), "population not arrived");
    Ok(())
}

/// One `gateway/<id>` stream, which every lane's claim calls name and whose view they wait on.
pub struct Gateway {
    stream: String,
    claims: watch::Receiver<Claims>,
    /// Held while a lane reads a fresh snapshot, since each subscription supersedes the gateway's previous stream.
    snapshots: Mutex<()>,
    _follower: AbortOnDropHandle<()>,
}

impl Gateway {
    pub async fn follow(connection: &sync::Connection) -> Result<Arc<Self>> {
        let (mut updates, stream, first) =
            subscribe(&mut sync::connect(&connection.endpoint).await?, connection).await?;
        let (sender, claims) = watch::channel(first);
        let follower = tokio::spawn(async move {
            let mut pending = Vec::new();
            while let Ok(Some(update)) = updates.message().await {
                let continued = update.continued;
                pending.push(update);
                if continued {
                    continue;
                }
                let mut applied = Ok(());
                sender.send_modify(|claims| applied = pending.drain(..).try_for_each(|update| apply(claims, update)));
                if let Err(error) = applied {
                    tracing::debug!(%error, "gateway topic stopped");
                    return;
                }
            }
        });
        Ok(Arc::new(Self { stream, claims, snapshots: Mutex::new(()), _follower: AbortOnDropHandle::new(follower) }))
    }
}

pub struct Client {
    rpc: CoreClient<Channel>,
    connection: sync::Connection,
    gateway: Arc<Gateway>,
}

impl Client {
    pub async fn connect(connection: &sync::Connection, gateway: Arc<Gateway>) -> Result<Self> {
        Ok(Self { rpc: sync::connect(&connection.endpoint).await?, connection: connection.clone(), gateway })
    }

    /// Calls `chunk:<method>` on the claim under `operation`, naming the gateway's stream.
    async fn call<R: Message + Default>(
        &mut self,
        method: &str,
        operation: &str,
        arguments: &impl Message,
    ) -> Result<R> {
        let message = CallRequest {
            operation_id: operation.into(),
            method: format!("chunk:{method}"),
            arguments: arguments.encode_to_vec(),
            stream: self.gateway.stream.clone(),
            ..CallRequest::default()
        };
        let (result, _) = sync::call(&mut self.rpc, &self.connection.gateway, message).await?;
        Ok(R::decode(result.as_slice())?)
    }

    /// Claims player `index`'s login, activates it and waits for the gateway's topic to show it arrived.
    pub async fn arrive(&mut self, index: u64) -> Result<String> {
        let (operation, login) = login(index);
        let claimed: ClaimResult = self.call("claim", &operation, &login).await?;
        match claimed.outcome {
            Some(claim_result::Outcome::Assignment(_)) => {}
            outcome => bail!("claim not assigned: {outcome:?}"),
        }
        let activated: ActivateResult = self.call("activate", &operation, &()).await?;
        ensure!(!activated.waiting, "activation waiting for a roster");
        let mut claims = self.gateway.claims.clone();
        claims
            .wait_for(|claims| claims.get(&operation) == Some(&ClaimPhase::Arrived))
            .await
            .context("gateway topic stopped")?;
        Ok(operation)
    }

    pub async fn execute(&mut self, sequence: u64, config: &Config) -> Result<()> {
        if config.scenario == Scenario::ControlPopulation {
            let _turn = self.gateway.snapshots.lock().await;
            let (_, _, claims) = subscribe(&mut self.rpc, &self.connection).await?;
            ensure!(claims.len() == config.population as usize, "incomplete snapshot");
        } else {
            let operation = self.arrive(sequence + u64::from(config.population)).await?;
            let departed: DepartResult = self.call("depart", &operation, &()).await?;
            ensure!(departed.departed, "synthetic player departure unconfirmed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[tokio::test(flavor = "multi_thread")]
    async fn population_lanes_reading_snapshots_at_once_do_not_supersede_each_other() {
        let state = tempfile::tempdir().unwrap();
        let config = Config::parse_from(["bench", "control-population", "--population", "4"]);
        let path = state.path().to_owned();
        let init =
            crate::target::Init { config: config.clone(), backend: String::new(), state: path.clone(), output: path };
        let core = crate::target::core(&init).await.unwrap();
        let connection = crate::target::connection(&core).unwrap();
        let gateway = Gateway::follow(&connection).await.unwrap();
        let mut seeder = Client::connect(&connection, gateway.clone()).await.unwrap();
        for index in 0..4 {
            seeder.arrive(index).await.unwrap();
        }
        let mut lanes = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let mut client = Client::connect(&connection, gateway.clone()).await.unwrap();
            let config = config.clone();
            lanes.spawn(async move {
                for sequence in 0..4 {
                    client.execute(sequence, &config).await?;
                }
                anyhow::Ok(())
            });
        }
        while let Some(lane) = lanes.join_next().await {
            lane.unwrap().unwrap();
        }
        core.stop(|| {}).await.unwrap();
    }
}
