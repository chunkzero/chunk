use std::sync::Arc;

use chunk_proto::v1::{
    ActivateClaim, Assignment, ClaimIdentity, ClaimRequest, ClaimUpdate, DesiredSessions, ProcessReport, WatchRequest,
    local_control_server::LocalControl,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

use crate::{Control, Error};

mod methods;
mod watch;

#[derive(Clone)]
pub struct Service {
    control: Arc<Control>,
    token: String,
    operations: crate::Operations,
    methods: Arc<methods::Methods>,
    watches: CancellationToken,
}

impl Service {
    /// # Errors
    /// Rejects an empty/short control credential.
    pub fn new(control: Arc<Control>, token: String) -> crate::Result<Self> {
        if token.len() < 32 {
            return Err(Error::Invalid("control credential too short"));
        }
        Ok(Self {
            control,
            token,
            operations: crate::Operations::default(),
            methods: Arc::default(),
            watches: CancellationToken::new(),
        })
    }

    pub(crate) fn close_methods(&self) {
        self.methods.close();
    }

    /// Ends every claim watch and JVM stream, so the transport can finish shutting down.
    pub(crate) fn close_watches(&self) {
        self.watches.cancel();
    }

    pub(crate) fn operations(&self) -> crate::Operations {
        self.operations.clone()
    }

    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if request.metadata().get("authorization").and_then(|v| v.to_str().ok())
            != Some(format!("Bearer {}", self.token).as_str())
        {
            return Err(Status::unauthenticated("invalid control credential"));
        }
        Ok(())
    }
}

pub(crate) fn status(error: Error) -> Status {
    match error {
        Error::Invalid(message) => Status::failed_precondition(message),
        Error::Capacity => Status::resource_exhausted("control capacity reached"),
        Error::Busy => Status::unavailable("control busy"),
        Error::Unresolved(message) => Status::unavailable(message),
        Error::Stopped => Status::failed_precondition("runtime stopped"),
        Error::Rpc(error) => error,
        other => {
            tracing::error!(error = %other, "control operation failed");
            Status::internal("control operation failed")
        }
    }
}

#[tonic::async_trait]
impl LocalControl for Service {
    async fn prepare_session_method(
        &self,
        request: Request<chunk_proto::v1::PrepareSessionMethodRequest>,
    ) -> Result<Response<chunk_proto::v1::PreparedMethodHandle>, Status> {
        self.authorize(&request)?;
        self.methods.prepare(&self.control, request.get_ref()).map(Response::new).map_err(status)
    }

    async fn start_prepared_method(
        &self,
        request: Request<chunk_proto::v1::PreparedMethodRequest>,
    ) -> Result<Response<chunk_proto::v1::SessionMethodResult>, Status> {
        self.authorize(&request)?;
        self.methods
            .start(&self.control, &self.operations.tracker, &request.get_ref().operation_id)
            .map(Response::new)
            .map_err(status)
    }

    async fn poll_prepared_method(
        &self,
        request: Request<chunk_proto::v1::PreparedMethodRequest>,
    ) -> Result<Response<chunk_proto::v1::SessionMethodResult>, Status> {
        self.authorize(&request)?;
        self.methods.poll(&request.get_ref().operation_id, false).map(Response::new).map_err(status)
    }

    async fn cancel_prepared_method(
        &self,
        request: Request<chunk_proto::v1::PreparedMethodRequest>,
    ) -> Result<Response<chunk_proto::v1::SessionMethodResult>, Status> {
        self.authorize(&request)?;
        self.methods.poll(&request.get_ref().operation_id, true).map(Response::new).map_err(status)
    }

    async fn nodes(
        &self,
        request: Request<chunk_proto::v1::NodesRequest>,
    ) -> Result<Response<chunk_proto::v1::NodeList>, Status> {
        self.authorize(&request)?;
        self.control.nodes().map(Response::new).map_err(status)
    }
    async fn players(
        &self,
        request: Request<chunk_proto::v1::PlayersRequest>,
    ) -> Result<Response<chunk_proto::v1::PlayerList>, Status> {
        self.authorize(&request)?;
        self.control.players().map(Response::new).map_err(status)
    }
    async fn shutdown_node(
        &self,
        request: Request<chunk_proto::v1::ShutdownNodeRequest>,
    ) -> Result<Response<chunk_proto::v1::NodeStatus>, Status> {
        self.authorize(&request)?;
        self.control.shutdown_node(request.get_ref()).map(Response::new).map_err(status)
    }

    async fn drain(
        &self,
        request: Request<chunk_proto::v1::DrainRequest>,
    ) -> Result<Response<chunk_proto::v1::DrainStatus>, Status> {
        self.authorize(&request)?;
        self.control.drain(request.into_inner()).map(Response::new).map_err(status)
    }
    async fn move_player(
        &self,
        request: Request<chunk_proto::v1::MovePlayerRequest>,
    ) -> Result<Response<ClaimRequest>, Status> {
        self.authorize(&request)?;
        self.control.move_player(request.into_inner()).map(Response::new).map_err(status)
    }

    type WatchStream = ReceiverStream<Result<ClaimUpdate, Status>>;

    async fn watch(&self, request: Request<WatchRequest>) -> Result<Response<Self::WatchStream>, Status> {
        self.authorize(&request)?;
        let proxy = request.into_inner().proxy_id;
        if proxy.is_empty() || proxy.len() > 128 {
            return Err(status(Error::Invalid("invalid proxy identity")));
        }
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let (control, closed) = (self.control.clone(), self.watches.clone());
        tokio::spawn(async move { control.watch(proxy, sender, closed).await });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn abandon_move(
        &self,
        request: Request<chunk_proto::v1::AbandonMoveRequest>,
    ) -> Result<Response<ClaimIdentity>, Status> {
        self.authorize(&request)?;
        let control = self.control.clone();
        self.operations
            .admit(async move { control.abandon_move(request.into_inner()).await })
            .await
            .map(Response::new)
            .map_err(status)
    }

    async fn claim(&self, request: Request<ClaimRequest>) -> Result<Response<Assignment>, Status> {
        self.authorize(&request)?;
        let control = self.control.clone();
        // A canceled RPC does not abandon an already durably reserved operation.
        self.operations
            .admit(async move { control.claim(request.into_inner()).await })
            .await
            .map(Response::new)
            .map_err(status)
    }

    async fn activate(&self, request: Request<ActivateClaim>) -> Result<Response<Assignment>, Status> {
        self.authorize(&request)?;
        let control = self.control.clone();
        self.operations
            .admit(async move { control.activate(request.into_inner()).await })
            .await
            .map(Response::new)
            .map_err(status)
    }

    async fn reconcile_departure(
        &self,
        request: Request<ClaimRequest>,
    ) -> Result<Response<chunk_proto::v1::DepartureStatus>, Status> {
        self.authorize(&request)?;
        let control = self.control.clone();
        self.operations
            .admit(async move { control.reconcile_departure(request.into_inner()).await })
            .await
            .map(Response::new)
            .map_err(status)
    }

    async fn cancel(&self, request: Request<ClaimRequest>) -> Result<Response<ClaimIdentity>, Status> {
        self.authorize(&request)?;
        let control = self.control.clone();
        self.operations
            .admit(async move { control.cancel(request.into_inner()).await })
            .await
            .map(Response::new)
            .map_err(status)
    }
}

fn process_credential<T>(request: &Request<T>) -> Result<String, Status> {
    Ok(request
        .metadata()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Status::unauthenticated("missing process credential"))?
        .to_owned())
}

#[tonic::async_trait]
impl chunk_proto::v1::supervisor_server::Supervisor for Service {
    async fn register_process(
        &self,
        request: Request<chunk_proto::v1::ProcessRegistration>,
    ) -> Result<Response<chunk_proto::v1::ProcessIdentity>, Status> {
        let token = process_credential(&request)?;
        self.control.register(&token, request.into_inner()).map(Response::new).map_err(status)
    }

    type SyncStream = ReceiverStream<Result<DesiredSessions, Status>>;

    async fn sync(
        &self,
        request: Request<tonic::Streaming<ProcessReport>>,
    ) -> Result<Response<Self::SyncStream>, Status> {
        let token = process_credential(&request)?;
        let token = token.strip_prefix("Bearer ").ok_or_else(|| Status::unauthenticated("process credential"))?.into();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let (control, closed) = (self.control.clone(), self.watches.clone());
        tokio::spawn(async move { control.sync(token, request.into_inner(), sender, closed).await });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}
