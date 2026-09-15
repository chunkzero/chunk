use std::sync::Arc;

use chunk_proto::v1::{ActivateClaim, Assignment, ClaimIdentity, ClaimRequest, local_control_server::LocalControl};
use tonic::{Request, Response, Status};

use crate::{Control, Error};

#[derive(Clone)]
pub struct Service {
    control: Arc<Control>,
    token: String,
    operations: tokio_util::task::TaskTracker,
}

impl Service {
    /// # Errors
    /// Rejects an empty/short process credential.
    pub fn new(control: Arc<Control>, token: String) -> crate::Result<Self> {
        if token.len() < 32 {
            return Err(Error::Invalid("control credential too short"));
        }
        Ok(Self { control, token, operations: tokio_util::task::TaskTracker::new() })
    }

    pub(crate) fn operations(&self) -> tokio_util::task::TaskTracker {
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

fn status(error: Error) -> Status {
    match error {
        Error::Invalid(message) => Status::failed_precondition(message),
        Error::Capacity => Status::resource_exhausted("control capacity reached"),
        Error::Unresolved(message) => Status::unavailable(message),
        Error::Stopped => Status::unavailable("runtime stopped"),
        Error::Rpc(error) => error,
        other => {
            tracing::error!(error = %other, "control operation failed");
            Status::internal("control operation failed")
        }
    }
}

#[tonic::async_trait]
impl LocalControl for Service {
    async fn nodes(
        &self,
        request: Request<chunk_proto::v1::NodesRequest>,
    ) -> Result<Response<chunk_proto::v1::NodeList>, Status> {
        self.authorize(&request)?;
        self.control.nodes().map(Response::new).map_err(status)
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

    async fn poll_move(
        &self,
        request: Request<ClaimRequest>,
    ) -> Result<Response<chunk_proto::v1::PendingMove>, Status> {
        self.authorize(&request)?;
        self.control.poll_move(request.get_ref()).map(Response::new).map_err(status)
    }

    async fn claim(&self, request: Request<ClaimRequest>) -> Result<Response<Assignment>, Status> {
        self.authorize(&request)?;
        let control = self.control.clone();
        // A canceled RPC does not abandon an already durably reserved operation.
        self.operations
            .spawn(async move { control.claim(request.into_inner()).await })
            .await
            .map_err(|_| Status::internal("claim task failed"))?
            .map(Response::new)
            .map_err(status)
    }

    async fn inspect(&self, request: Request<ClaimRequest>) -> Result<Response<Assignment>, Status> {
        self.authorize(&request)?;
        self.control.inspect(request.into_inner()).await.map(Response::new).map_err(status)
    }

    async fn activate(&self, request: Request<ActivateClaim>) -> Result<Response<Assignment>, Status> {
        self.authorize(&request)?;
        let control = self.control.clone();
        self.operations
            .spawn(async move { control.activate(request.into_inner()).await })
            .await
            .map_err(|_| Status::internal("activation task failed"))?
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
            .spawn(async move { control.reconcile_departure(request.into_inner()).await })
            .await
            .map_err(|_| Status::internal("departure task failed"))?
            .map(Response::new)
            .map_err(status)
    }

    async fn cancel(&self, request: Request<ClaimRequest>) -> Result<Response<ClaimIdentity>, Status> {
        self.authorize(&request)?;
        let control = self.control.clone();
        self.operations
            .spawn(async move { control.cancel(request.into_inner()).await })
            .await
            .map_err(|_| Status::internal("cancellation task failed"))?
            .map(Response::new)
            .map_err(status)
    }
}

#[tonic::async_trait]
impl chunk_proto::v1::supervisor_server::Supervisor for Service {
    async fn register_process(
        &self,
        request: Request<chunk_proto::v1::ProcessRegistration>,
    ) -> Result<Response<chunk_proto::v1::ProcessIdentity>, Status> {
        let token = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Status::unauthenticated("missing process credential"))?
            .to_owned();
        self.control.host.register(&token, request.into_inner()).map(Response::new).map_err(status)
    }
}
