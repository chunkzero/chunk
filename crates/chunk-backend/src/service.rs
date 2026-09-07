use chunk_proto::v1::{BackendCall, BackendResult, BackendUpdate, BackendWatch, backend_server};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::{Backend, Call, Error};

#[derive(Clone)]
pub struct Service {
    backend: Backend,
    credential: String,
}

impl Service {
    /// # Errors
    /// Requires a nonempty opaque credential suitable for authorization metadata.
    pub fn new(backend: Backend, credential: &str) -> crate::Result<Self> {
        if credential.len() < 32
            || credential.len() > 256
            || !credential
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Invalid("backend credential"));
        }
        Ok(Self {
            backend,
            credential: format!("Bearer {credential}"),
        })
    }

    #[must_use]
    pub fn into_server(self) -> backend_server::BackendServer<Self> {
        backend_server::BackendServer::new(self)
            .max_decoding_message_size(2 * 1024 * 1024)
            .max_encoding_message_size(2 * 1024 * 1024)
    }

    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if request.metadata().get("authorization").and_then(|v| v.to_str().ok()) != Some(&self.credential) {
            return Err(Status::unauthenticated("invalid backend credential"));
        }
        Ok(())
    }

    fn decode(&self, call: BackendCall) -> Result<Call, Status> {
        if call.environment != self.backend.environment() {
            return Err(Status::permission_denied("environment mismatch"));
        }
        Ok(Call {
            deployment: call.deployment,
            function: call.function,
            operation: call.operation_id,
            arguments: serde_json::from_slice(&call.arguments_json)
                .map_err(|_| Status::invalid_argument("arguments JSON"))?,
            caller: serde_json::from_slice(&call.caller_json).map_err(|_| Status::invalid_argument("caller JSON"))?,
        })
    }
}

fn status(error: &Error) -> Status {
    match error {
        Error::Invalid(_) | Error::Contract | Error::Json(_) => Status::invalid_argument(error.to_string()),
        Error::Unknown => Status::not_found(error.to_string()),
        Error::Busy => Status::resource_exhausted(error.to_string()),
        Error::Cancelled | Error::JavaScript(chunk_js::Error::Cancelled) => Status::cancelled("invocation cancelled"),
        Error::JavaScript(chunk_js::Error::Deadline) => Status::deadline_exceeded("invocation deadline"),
        Error::Store(chunk_store::Error::OperationMismatch) => Status::already_exists(error.to_string()),
        Error::JavaScript(_) => Status::failed_precondition(error.to_string()),
        _ => Status::internal("environment backend failure"),
    }
}

#[tonic::async_trait]
impl backend_server::Backend for Service {
    async fn call(&self, request: Request<BackendCall>) -> Result<Response<BackendResult>, Status> {
        self.authorize(&request)?;
        let outcome = self
            .backend
            .call(self.decode(request.into_inner())?)
            .await
            .map_err(|error| status(&error))?;
        Ok(Response::new(BackendResult {
            revision: outcome.revision.0,
            result_json: serde_json::to_vec(&outcome.result).map_err(|_| Status::internal("result encoding"))?,
        }))
    }

    type WatchStream = ReceiverStream<Result<BackendUpdate, Status>>;

    async fn watch(&self, request: Request<BackendWatch>) -> Result<Response<Self::WatchStream>, Status> {
        self.authorize(&request)?;
        let calls = request
            .into_inner()
            .queries
            .into_iter()
            .map(|call| self.decode(call))
            .collect::<Result<_, _>>()?;
        let mut group = self.backend.subscribe(calls).map_err(|error| status(&error))?;
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    () = sender.closed() => break,
                    result = group.next() => result,
                };
                let result = result
                    .and_then(|update| {
                        Ok(BackendUpdate {
                            revision: update.revision.0,
                            results_json: update
                                .results
                                .iter()
                                .map(serde_json::to_vec)
                                .collect::<Result<_, _>>()?,
                        })
                    })
                    .map_err(|error| status(&error));
                let failed = result.is_err();
                if sender.send(result).await.is_err() || failed {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}
