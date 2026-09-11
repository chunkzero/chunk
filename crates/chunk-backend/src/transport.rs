use chunk_proto::v1::{BackendMutation, BackendQuery, BackendResult, BackendUpdate, BackendWatchGroup, backend_server};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::{Backend, Call, Error};

/// Authenticated platform transport. Only trusted processes may assert caller
/// context; application-provided arguments never become caller authority.
#[derive(Clone)]
pub struct Service {
    backend: Backend,
    credential: String,
    workers: tokio_util::task::TaskTracker,
    shutdown: tokio_util::sync::CancellationToken,
}

impl Service {
    /// # Errors
    /// Requires a nonempty opaque credential suitable for authorization metadata.
    pub fn new(backend: Backend, credential: &str) -> crate::Result<Self> {
        if !(32..=256).contains(&credential.len())
            || !credential.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Invalid("backend credential"));
        }
        Ok(Self {
            backend,
            workers: tokio_util::task::TaskTracker::new(),
            shutdown: tokio_util::sync::CancellationToken::new(),
            credential: format!("Bearer {credential}"),
        })
    }

    #[must_use]
    pub fn into_server(self) -> backend_server::BackendServer<Self> {
        backend_server::BackendServer::new(self)
            .max_decoding_message_size(2 * 1024 * 1024)
            .max_encoding_message_size(2 * 1024 * 1024)
    }

    pub(crate) fn workers(&self) -> tokio_util::task::TaskTracker {
        self.workers.clone()
    }
    pub(crate) fn shutdown(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown.clone()
    }

    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let valid = request.metadata().get("authorization").and_then(|v| v.to_str().ok()).is_some_and(|value| {
            value.len() == self.credential.len()
                && value
                    .bytes()
                    .zip(self.credential.bytes())
                    .fold(0, |difference, (a, b)| difference | std::hint::black_box(a ^ b))
                    == 0
        });
        if !valid {
            return Err(Status::unauthenticated("invalid backend credential"));
        }
        Ok(())
    }

    fn binding<T>(&self, request: &Request<T>) -> Result<chunk_js::DeploymentId, Status> {
        self.authorize(request)?;
        let metadata = request.metadata();
        let environment = metadata
            .get("x-chunk-environment")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Status::invalid_argument("missing environment binding"))?;
        if environment != self.backend.environment() {
            return Err(Status::permission_denied("environment mismatch"));
        }
        let deployment = metadata
            .get("x-chunk-deployment")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Status::invalid_argument("missing deployment binding"))?;
        chunk_js::DeploymentId::new(deployment).map_err(|_| Status::invalid_argument("deployment"))
    }

    fn decode(
        deployment: chunk_js::DeploymentId,
        function: String,
        arguments: &[u8],
        caller: &[u8],
    ) -> Result<Call, Status> {
        let json = |bytes: &[u8]| {
            let text = std::str::from_utf8(bytes).map_err(|_| Status::invalid_argument("JSON UTF-8"))?;
            chunk_js::Json::parse(text).map_err(|_| Status::invalid_argument("invalid JSON"))
        };
        Ok(Call { deployment, function, arguments: json(arguments)?, caller: json(caller)? })
    }
}

fn status(error: &Error) -> Status {
    match error {
        Error::Invalid(_) | Error::Contract | Error::Json(_) => Status::invalid_argument(error.to_string()),
        Error::Unknown => Status::not_found(error.to_string()),
        Error::Busy => Status::resource_exhausted(error.to_string()),
        Error::Retry => Status::aborted(error.to_string()),
        Error::Cancelled => Status::cancelled("invocation cancelled"),
        Error::JavaScript(error) => match error.as_ref() {
            chunk_js::Error::Cancelled => Status::cancelled("invocation cancelled"),
            chunk_js::Error::Deadline => Status::deadline_exceeded("invocation deadline"),
            _ => Status::failed_precondition(error.to_string()),
        },
        Error::OperationMismatch => Status::already_exists(error.to_string()),
        Error::Storage(inner) if matches!(inner.as_ref(), chunk_store::Error::OperationMismatch) => {
            Status::already_exists(error.to_string())
        }
        _ => Status::unavailable("environment backend unavailable; recover mutation by operation ID"),
    }
}

#[tonic::async_trait]
impl backend_server::Backend for Service {
    async fn check_deployment(&self, request: Request<()>) -> Result<Response<()>, Status> {
        let deployment = self.binding(&request)?;
        self.backend.check_deployment(deployment).await.map_err(|error| status(&error))?;
        Ok(Response::new(()))
    }

    async fn query(&self, request: Request<BackendQuery>) -> Result<Response<BackendResult>, Status> {
        let deployment = self.binding(&request)?;
        let query = request.into_inner();
        let call = Self::decode(deployment, query.function, &query.arguments_json, &query.caller_json)?;
        let outcome = self.backend.query(call).await.map_err(|error| status(&error))?;
        Ok(Response::new(BackendResult { revision: outcome.revision.0, result_json: outcome.json.as_bytes().to_vec() }))
    }

    async fn mutate(&self, request: Request<BackendMutation>) -> Result<Response<BackendResult>, Status> {
        let deployment = self.binding(&request)?;
        let mutation = request.into_inner();
        if mutation.operation_id.is_empty() {
            return Err(Status::invalid_argument("mutation requires an operation ID"));
        }
        let call = Self::decode(deployment, mutation.function, &mutation.arguments_json, &mutation.caller_json)?;
        let outcome = self.backend.mutate(mutation.operation_id, call).await.map_err(|error| status(&error))?;
        Ok(Response::new(BackendResult { revision: outcome.revision.0, result_json: outcome.json.as_bytes().to_vec() }))
    }

    type WatchGroupStream = ReceiverStream<Result<BackendUpdate, Status>>;

    async fn watch_group(
        &self,
        request: Request<BackendWatchGroup>,
    ) -> Result<Response<Self::WatchGroupStream>, Status> {
        let deployment = self.binding(&request)?;
        let calls = request
            .into_inner()
            .queries
            .into_iter()
            .map(|call| Self::decode(deployment.clone(), call.function, &call.arguments_json, &call.caller_json))
            .collect::<Result<_, _>>()?;
        let mut group = self.backend.subscribe_group(calls).await.map_err(|error| status(&error))?;
        let (sender, receiver) = mpsc::channel(1);
        let shutdown = self.shutdown.clone();
        self.workers.spawn(async move {
            loop {
                let result = tokio::select! {
                    () = sender.closed() => break,
                    () = shutdown.cancelled() => break,
                    result = group.next() => result,
                };
                let result = result
                    .map(|update| {
                        let (results_json, errors) = update
                            .results
                            .into_iter()
                            .map(|result| match result {
                                Ok(json) => (json.as_bytes().to_vec(), String::new()),
                                Err(error) => (Vec::new(), error.to_string()),
                            })
                            .unzip();
                        BackendUpdate { revision: update.revision.0, results_json, errors }
                    })
                    .map_err(|error| status(&error));
                let failed = result.is_err();
                let sent = tokio::select! { () = shutdown.cancelled() => break, sent = sender.send(result) => sent };
                if sent.is_err() || failed {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}
