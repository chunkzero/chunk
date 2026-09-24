use std::{collections::BTreeSet, sync::Mutex};

use anyhow::{Result, ensure};
use chunk_control::{ControlConnection, Host, RuntimeConnection};
use chunk_proto::v1::{
    ActivateClaim, ClaimPhase, ClaimRequest, Identity, SessionDemand, local_control_client::LocalControlClient,
};
use tonic::{Request, transport::Channel};

use crate::{
    config::{Config, Scenario},
    fixtures,
};

pub struct SyntheticHost {
    endpoint: String,
    stopped: Mutex<BTreeSet<String>>,
}

impl SyntheticHost {
    pub fn new(endpoint: String) -> Self {
        Self { endpoint, stopped: Mutex::default() }
    }
}

#[tonic::async_trait]
impl Host for SyntheticHost {
    async fn ensure(&self, id: &str, _: &str, _: &str) -> chunk_control::Result<RuntimeConnection> {
        if self.stopped(id) {
            return Err(chunk_control::Error::Stopped);
        }
        Ok(self.connection(id).expect("synthetic connection"))
    }

    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        Some(RuntimeConnection {
            endpoint: self.endpoint.clone(),
            token: format!("bench-{id}"),
            player_endpoint: "127.0.0.1:1".into(),
            identity: fixtures::identity(id),
        })
    }

    async fn terminate(&self, id: &str) -> chunk_control::Result<()> {
        self.stopped.lock().expect("synthetic host lock").insert(id.into());
        Ok(())
    }

    fn stopped(&self, id: &str) -> bool {
        self.stopped.lock().expect("synthetic host lock").contains(id)
    }
}

pub fn configuration() -> Result<chunk_control::Config> {
    Ok(serde_json::from_value(serde_json::json!({
        "apps": {"bench": {"id":"bench", "jar":"bench.jar", "sha256":"bench", "java_version":25,
            "sessions":{"default":{"machine_profile":"bench","capacity":128}}}},
        "deployment":{"environment":"bench","deployment":"bench"}, "artifact_digest":"bench",
        "profiles":{"bench":{"memory_mib":512,"max_sessions":1}},
        "session_types":{"bench/default":{"app":"bench","machine_profile":"bench","capacity":128}},
        "max_processes":32, "idle_node_timeout_seconds":0
    }))?)
}

pub fn claim(index: u64) -> ClaimRequest {
    ClaimRequest {
        operation_id: format!("bench-{index:08}"),
        proxy_id: "bench-proxy".into(),
        connection_id: format!("connection-{index:08}"),
        identity: Some(Identity {
            uuid: uuid::Uuid::from_u128(u128::from(index) + 1).to_string(),
            username: "bench".into(),
            properties: vec![],
        }),
        demand: Some(SessionDemand {
            key: "lobby".into(),
            session_type: "bench/default".into(),
            machine_profile: "bench".into(),
        }),
        source: None,
    }
}

pub struct Client {
    client: LocalControlClient<Channel>,
    authorization: tonic::metadata::MetadataValue<tonic::metadata::Ascii>,
}

impl Client {
    pub async fn connect(connection: &ControlConnection) -> Result<Self> {
        Ok(Self {
            client: LocalControlClient::connect(connection.endpoint.clone()).await?,
            authorization: format!("Bearer {}", connection.token).parse()?,
        })
    }

    fn request<T>(&self, body: T) -> Request<T> {
        let mut request = Request::new(body);
        request.metadata_mut().insert("authorization", self.authorization.clone());
        request
    }

    pub async fn arrive(&mut self, request: ClaimRequest) -> Result<()> {
        let assignment = self.client.claim(self.request(request)).await?.into_inner();
        ensure!(assignment.claim.is_some() && assignment.preparation.is_some(), "incomplete assignment");
        let arrived = self.client.activate(self.request(ActivateClaim { claim: assignment.claim })).await?.into_inner();
        ensure!(arrived.phase == ClaimPhase::Arrived as i32, "synthetic player did not arrive");
        Ok(())
    }

    pub async fn execute(&mut self, sequence: u64, config: &Config) -> Result<()> {
        if config.scenario == Scenario::ControlPopulation {
            let request = claim(sequence % u64::from(config.population));
            ensure!(
                self.client.poll_move(self.request(request)).await?.into_inner().claim.is_none(),
                "unexpected move"
            );
        } else {
            let request = claim(sequence + u64::from(config.population));
            self.arrive(request.clone()).await?;
            let departure = self.client.reconcile_departure(self.request(request)).await?.into_inner();
            ensure!(departure.departed, "synthetic player departure unconfirmed");
        }
        Ok(())
    }

    pub async fn verify_population(&mut self, expected: u32) -> Result<()> {
        let players = self.client.players(self.request(chunk_proto::v1::PlayersRequest {})).await?.into_inner();
        ensure!(players.players.len() == expected as usize, "seeded population differs from requested population");
        ensure!(
            players.players.iter().all(|player| player.phase == ClaimPhase::Arrived as i32),
            "population not arrived"
        );
        Ok(())
    }
}
