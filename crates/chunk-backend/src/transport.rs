use chunk_proto::v1::{BackendCall, BackendResult, BackendUpdate, BackendWatch, backend_server};
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
}

impl Service {
    /// # Errors
    /// Requires a nonempty opaque credential suitable for authorization metadata.
    pub fn new(backend: Backend, credential: &str) -> crate::Result<Self> {
        if !(32..=256).contains(&credential.len())
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
        let valid = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|value| {
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

    fn decode(&self, call: BackendCall) -> Result<(Call, String), Status> {
        if call.environment != self.backend.environment() {
            return Err(Status::permission_denied("environment mismatch"));
        }
        let json = |bytes: &[u8]| {
            let text = std::str::from_utf8(bytes).map_err(|_| Status::invalid_argument("JSON UTF-8"))?;
            chunk_js::Json::parse(text).map_err(|_| Status::invalid_argument("invalid JSON"))
        };
        Ok((
            Call {
                deployment: chunk_js::DeploymentId::new(call.deployment)
                    .map_err(|_| Status::invalid_argument("deployment"))?,
                function: call.function,
                arguments: json(&call.arguments_json)?,
                caller: json(&call.caller_json)?,
            },
            call.operation_id,
        ))
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
    async fn call(&self, request: Request<BackendCall>) -> Result<Response<BackendResult>, Status> {
        self.authorize(&request)?;
        let (call, operation) = self.decode(request.into_inner())?;
        let outcome = if operation.is_empty() {
            self.backend.query(call).await
        } else {
            self.backend.mutate(operation, call).await
        }
        .map_err(|error| status(&error))?;
        Ok(Response::new(BackendResult {
            revision: outcome.revision.0,
            result_json: outcome.json.as_bytes().to_vec(),
        }))
    }

    type WatchStream = ReceiverStream<Result<BackendUpdate, Status>>;

    async fn watch(&self, request: Request<BackendWatch>) -> Result<Response<Self::WatchStream>, Status> {
        self.authorize(&request)?;
        let calls = request
            .into_inner()
            .queries
            .into_iter()
            .map(|call| {
                let (call, operation) = self.decode(call)?;
                if !operation.is_empty() {
                    return Err(Status::invalid_argument("watch operation ID"));
                }
                Ok(call)
            })
            .collect::<Result<_, _>>()?;
        let mut group = self
            .backend
            .subscribe_group(calls)
            .await
            .map_err(|error| status(&error))?;
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    () = sender.closed() => break,
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
                        BackendUpdate {
                            revision: update.revision.0,
                            results_json,
                            errors,
                        }
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
