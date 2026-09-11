use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

use chunk_proto::v1::{
    ConfigurationRequest, ConfigurationResponse, PlayerDelivery, PlayerPreparation, PlayerWithdrawal, ProcessIdentity,
    ProcessInventory, ProcessRegistration, SessionCommand, SessionInventory, gameplay_client::GameplayClient,
    gameplay_server, process_control_client::ProcessControlClient, process_control_server, supervisor_server,
};
use prost::Message;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, transport::Channel};

use crate::{Phase, Status as RuntimeStatus, relay::Binding};

#[derive(Clone)]
pub(crate) struct Registered {
    pub registration: ProcessRegistration,
    pub channel: Channel,
}

pub(crate) struct Shared {
    pub identity: ProcessIdentity,
    pub child_credential: String,
    pub credential: String,
    pub ingress: SocketAddr,
    pub registration: Mutex<Option<Registered>>,
    pub bindings: Mutex<BTreeMap<String, Arc<Binding>>>,
    pub shutdown: CancellationToken,
    pub status: watch::Sender<RuntimeStatus>,
}

impl Shared {
    pub fn registered(&self) -> Result<Registered, Status> {
        self.registration
            .lock()
            .map_err(|_| Status::internal("registration poisoned"))?
            .clone()
            .ok_or_else(|| Status::unavailable("JVM has not registered"))
    }

    pub fn request<T>(&self, body: T) -> Request<T> {
        let mut request = Request::new(body);
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.child_credential).parse().expect("generated credential"),
        );
        request.set_timeout(Duration::from_secs(3));
        request
    }

    fn authorize<T>(&self, request: &Request<T>, child: bool) -> Result<(), Status> {
        let credential = if child { &self.child_credential } else { &self.credential };
        if request.metadata().get("authorization").and_then(|v| v.to_str().ok())
            != Some(format!("Bearer {credential}").as_str())
        {
            return Err(Status::unauthenticated("invalid process credential"));
        }
        Ok(())
    }

    fn identity(&self, identity: &ProcessIdentity) -> Result<(), Status> {
        if identity != &self.identity {
            return Err(Status::failed_precondition("stale process identity"));
        }
        Ok(())
    }

    pub async fn inventory(&self) -> Result<ProcessInventory, Status> {
        let registered = self.registered()?;
        let inventory = ProcessControlClient::new(registered.channel)
            .max_decoding_message_size(8 * 1024 * 1024)
            .inventory(self.request(self.identity.clone()))
            .await?
            .into_inner();
        self.identity(
            inventory.identity.as_ref().ok_or_else(|| Status::failed_precondition("missing inventory identity"))?,
        )?;
        Ok(inventory)
    }
}

#[derive(Clone)]
pub(crate) struct Service(pub Arc<Shared>);

fn loopback(endpoint: &str) -> Result<SocketAddr, Status> {
    let address: SocketAddr = endpoint.parse().map_err(|_| Status::invalid_argument("invalid local endpoint"))?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(Status::invalid_argument("endpoint must be loopback"));
    }
    Ok(address)
}

#[tonic::async_trait]
impl supervisor_server::Supervisor for Service {
    async fn register_process(
        &self,
        request: Request<ProcessRegistration>,
    ) -> Result<Response<ProcessIdentity>, Status> {
        self.0.authorize(&request, true)?;
        let registration = request.into_inner();
        self.0.identity(registration.identity.as_ref().ok_or_else(|| Status::invalid_argument("missing identity"))?)?;
        let config =
            registration.configuration.as_ref().ok_or_else(|| Status::invalid_argument("missing configuration"))?;
        if config.deployment != self.0.identity.deployment
            || config.runtime_id != self.0.identity.runtime_id
            || config.process_generation != self.0.identity.generation
            || config.encoded_len() > 65_536
            || config.protocol <= 0
        {
            return Err(Status::failed_precondition("invalid configuration identity"));
        }
        let endpoint = loopback(&registration.control_endpoint)?;
        loopback(&registration.player_endpoint)?;
        if let Some(previous) =
            self.0.registration.lock().map_err(|_| Status::internal("registration poisoned"))?.as_ref()
        {
            if previous.registration != registration {
                return Err(Status::failed_precondition("registration is immutable"));
            }
            return Ok(Response::new(self.0.identity.clone()));
        }
        let channel = Channel::from_shared(format!("http://{endpoint}"))
            .map_err(|_| Status::invalid_argument("endpoint"))?
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(3))
            .connect()
            .await
            .map_err(|_| Status::unavailable("JVM endpoint unavailable"))?;
        let mut current = self.0.registration.lock().map_err(|_| Status::internal("registration poisoned"))?;
        if current.as_ref().is_some_and(|previous| previous.registration != registration) {
            return Err(Status::failed_precondition("registration is immutable"));
        }
        if current.is_none() {
            *current = Some(Registered { registration, channel });
        }
        Ok(Response::new(self.0.identity.clone()))
    }
}

#[tonic::async_trait]
impl gameplay_server::Gameplay for Service {
    async fn withdraw_player(&self, request: Request<PlayerWithdrawal>) -> Result<Response<PlayerWithdrawal>, Status> {
        self.0.authorize(&request, false)?;
        let withdrawal = request.into_inner();
        let binding = self
            .0
            .bindings
            .lock()
            .map_err(|_| Status::internal("bindings poisoned"))?
            .get(&withdrawal.operation_id)
            .cloned()
            .ok_or_else(|| Status::not_found("unknown delivery"))?;
        if binding.delivery.owner_generation != withdrawal.owner_generation {
            return Err(Status::failed_precondition("stale owner generation"));
        }
        let result =
            GameplayClient::new(binding.registered.channel.clone()).withdraw_player(self.0.request(withdrawal)).await?;
        binding.closed.store(true, Ordering::Release);
        Ok(result)
    }

    async fn configuration(
        &self,
        request: Request<ConfigurationRequest>,
    ) -> Result<Response<ConfigurationResponse>, Status> {
        self.0.authorize(&request, false)?;
        if request.get_ref().deployment != self.0.identity.deployment {
            return Err(Status::permission_denied("deployment mismatch"));
        }
        if self.0.status.borrow().phase != Phase::Ready {
            return Err(Status::unavailable("JVM not ready"));
        }
        Ok(Response::new(
            self.0.registered()?.registration.configuration.ok_or_else(|| Status::internal("missing configuration"))?,
        ))
    }

    async fn prepare_player(&self, request: Request<PlayerDelivery>) -> Result<Response<PlayerPreparation>, Status> {
        self.0.authorize(&request, false)?;
        if self.0.shutdown.is_cancelled() {
            return Err(Status::unavailable("runtime stopping"));
        }
        let delivery = request.into_inner();
        if delivery.deployment != self.0.identity.deployment
            || delivery.runtime_id != self.0.identity.runtime_id
            || delivery.process_generation != self.0.identity.generation
            || delivery.operation_id.is_empty()
            || delivery.operation_id.len() > 128
            || delivery.encoded_len() > 65_536
        {
            return Err(Status::failed_precondition("invalid delivery identity"));
        }
        let binding = {
            let mut bindings = self.0.bindings.lock().map_err(|_| Status::internal("bindings poisoned"))?;
            if let Some(binding) = bindings.get(&delivery.operation_id) {
                if binding.delivery != delivery || binding.closed.load(Ordering::Acquire) {
                    return Err(Status::failed_precondition("delivery operation changed or closed"));
                }
                binding.clone()
            } else {
                if self.0.status.borrow().phase != Phase::Ready {
                    return Err(Status::unavailable("JVM not ready"));
                }
                if bindings.len() >= 4096 {
                    return Err(Status::resource_exhausted("delivery history full"));
                }
                let binding = Arc::new(Binding::new(delivery.clone(), self.0.registered()?));
                bindings.insert(delivery.operation_id.clone(), binding.clone());
                binding
            }
        };
        binding
            .downstream
            .get_or_try_init(|| async {
                let preparation = GameplayClient::new(binding.registered.channel.clone())
                    .prepare_player(self.0.request(delivery))
                    .await?
                    .into_inner();
                if preparation.endpoint != binding.registered.registration.player_endpoint
                    || preparation.operation_id != binding.delivery.operation_id
                    || preparation.capability.len() != 32
                {
                    return Err(Status::failed_precondition("invalid JVM preparation"));
                }
                Ok(preparation)
            })
            .await?;
        Ok(Response::new(binding.result(&self.0)))
    }
}

#[tonic::async_trait]
impl process_control_server::ProcessControl for Service {
    async fn create_session(&self, request: Request<SessionCommand>) -> Result<Response<SessionInventory>, Status> {
        self.0.authorize(&request, false)?;
        self.0.identity(
            request.get_ref().identity.as_ref().ok_or_else(|| Status::invalid_argument("missing identity"))?,
        )?;
        if self.0.status.borrow().phase != Phase::Ready || self.0.shutdown.is_cancelled() {
            return Err(Status::unavailable("runtime not ready"));
        }
        ProcessControlClient::new(self.0.registered()?.channel)
            .create_session(self.0.request(request.into_inner()))
            .await
    }

    async fn finish_session(&self, request: Request<SessionCommand>) -> Result<Response<SessionInventory>, Status> {
        self.0.authorize(&request, false)?;
        self.0.identity(
            request.get_ref().identity.as_ref().ok_or_else(|| Status::invalid_argument("missing identity"))?,
        )?;
        ProcessControlClient::new(self.0.registered()?.channel)
            .finish_session(self.0.request(request.into_inner()))
            .await
    }

    async fn inventory(&self, request: Request<ProcessIdentity>) -> Result<Response<ProcessInventory>, Status> {
        self.0.authorize(&request, false)?;
        self.0.identity(request.get_ref())?;
        Ok(Response::new(self.0.inventory().await?))
    }

    async fn stop_process(&self, request: Request<ProcessIdentity>) -> Result<Response<ProcessIdentity>, Status> {
        self.0.authorize(&request, false)?;
        self.0.identity(request.get_ref())?;
        let mut status = self.0.status.subscribe();
        self.0.shutdown.cancel();
        status
            .wait_for(|status| matches!(status.phase, Phase::Stopped | Phase::Failed))
            .await
            .map_err(|_| Status::unavailable("runtime stopped"))?;
        Ok(Response::new(self.0.identity.clone()))
    }
}
