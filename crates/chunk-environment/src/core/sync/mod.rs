//! The sync protocol's `Core` service, served on control's listener: app function calls, platform methods and topic
//! subscriptions for gateways, JVMs and the CLI.

mod app;
mod auth;
mod caller;
mod errors;
mod platform;
mod runs;
mod streams;
mod topics;

use chunk_backend::{Backend, Call, Update as Outcome};
use chunk_control::Control;
use chunk_js::{DeploymentId, Json};
use chunk_proto::sync::v1::{
    CallRequest, CallResponse, Caller, Error, Position, SubscribeRequest, call_response,
    core_server::{Core, CoreServer},
};
use chunk_store::Revision;
use prost::Message;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// Every message's size limit.
const MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const OPERATION_BYTES: usize = 256;
/// Methods, topics, deployments, keys, stream IDs and caller fields.
const NAME_BYTES: usize = 512;
const ARGUMENT_BYTES: usize = 1024 * 1024;

pub(crate) use auth::Gateways;

/// Serves `chunk.sync.v1.Core` beside control, running app functions on `backend`. Each gateway presents the
/// credential `gateways` minted for it, the CLI presents control's credential, and each JVM its process credential.
pub(crate) fn services(backend: Backend, gateways: Arc<Gateways>) -> chunk_control::server::Services {
    Box::new(move |control, token, stop, operations| {
        let service = SyncService {
            credentials: Arc::new(auth::Credentials {
                gateways: gateways.clone(),
                cli: token.to_owned(),
                control: control.clone(),
            }),
            control: control.clone(),
            epoch: backend.system().epoch().0,
            app: app::App::new(backend),
            streams: streams::StreamKey::new(),
            fences: streams::Fences::default(),
            runs: Arc::default(),
            stop,
            operations,
        };
        let server =
            CoreServer::new(service).max_decoding_message_size(MESSAGE_BYTES).max_encoding_message_size(MESSAGE_BYTES);
        tonic::service::Routes::new(server)
    })
}

pub(crate) struct SyncService {
    credentials: Arc<auth::Credentials>,
    control: Arc<Control>,
    app: app::App,
    streams: streams::StreamKey,
    fences: streams::Fences,
    runs: Arc<runs::Runs>,
    /// The store's epoch, fixed while the backend runs.
    epoch: u64,
    /// Ends open streams when control's transport shuts down.
    stop: CancellationToken,
    operations: chunk_control::Operations,
}

impl SyncService {
    /// The deployment and caller an app function runs with, checked against the principal.
    fn scope(
        &self,
        principal: &auth::Principal,
        deployment: &str,
        caller: Option<&Caller>,
    ) -> Result<(DeploymentId, Json), Error> {
        let id = DeploymentId::new(deployment).map_err(|_| errors::invalid("invalid deployment"))?;
        Ok((id, caller::derive(&self.control, &principal.class, deployment, caller)?))
    }

    /// Runs `request`'s app function or platform method, returning its encoded result and the position it committed at
    /// or observed.
    async fn dispatch(
        &self,
        principal: &auth::Principal,
        request: CallRequest,
    ) -> Result<(Option<Position>, Vec<u8>), Error> {
        check_names(&[&request.method, &request.deployment, &request.stream], request.caller.as_ref())?;
        if request.operation_id.len() > OPERATION_BYTES {
            return Err(errors::invalid("the operation ID exceeds 256 bytes"));
        }
        if request.arguments.len() > ARGUMENT_BYTES {
            return Err(errors::invalid("arguments exceed 1 MiB"));
        }
        if matches!(principal.class, auth::Class::Unadopted { .. }) && request.method != "chunk:register" {
            return Err(errors::denied("the JVM must register again first"));
        }
        if let Some(method) = request.method.strip_prefix("chunk:").map(str::to_owned) {
            return platform::call(self, principal, &method, request).await;
        }
        let outcome = self.call_app(principal, request).await?;
        Ok((position(self.epoch, outcome.revision), outcome.json.as_bytes().to_vec()))
    }

    async fn call_app(&self, principal: &auth::Principal, request: CallRequest) -> Result<Outcome, Error> {
        if request.method.is_empty() || request.method.contains(':') {
            return Err(errors::invalid("unknown method"));
        }
        if !request.stream.is_empty() {
            self.fences.check(&request.stream, &principal.credential)?;
        }
        let (deployment, caller) = self.scope(principal, &request.deployment, request.caller.as_ref())?;
        let arguments = std::str::from_utf8(&request.arguments).ok().and_then(|text| Json::parse(text).ok());
        let arguments = arguments.ok_or_else(|| errors::invalid("arguments are not JSON"))?;
        let call = Call { deployment, function: request.method, arguments, caller };
        self.app.call(principal, request.operation_id, call).await
    }

    async fn open(&self, principal: auth::Principal, request: &SubscribeRequest) -> Result<topics::Topic, Error> {
        let after = request.after.as_ref().map_or("", |after| after.stream.as_str());
        check_names(&[&request.topic, &request.deployment, after], request.caller.as_ref())?;
        if request.arguments.len() > ARGUMENT_BYTES {
            return Err(errors::invalid("arguments exceed 1 MiB"));
        }
        topics::Topic::open(self, principal, request).await
    }
}

fn check_names(names: &[&str], caller: Option<&Caller>) -> Result<(), Error> {
    let caller = caller.map(|caller| [caller.session.as_str(), caller.player.as_str()]).unwrap_or_default();
    if names.iter().chain(&caller).any(|name| name.len() > NAME_BYTES) {
        return Err(errors::invalid("a name exceeds 512 bytes"));
    }
    Ok(())
}

/// The position of `revision` in `epoch`; revision zero names no commit.
fn position(epoch: u64, revision: Revision) -> Option<Position> {
    (revision.0 != 0).then_some(Position { epoch, revision: revision.0 })
}

#[tonic::async_trait]
impl Core for SyncService {
    async fn call(&self, request: Request<CallRequest>) -> Result<Response<CallResponse>, Status> {
        let principal = self.credentials.authenticate(&request)?;
        let response = match self.dispatch(&principal, request.into_inner()).await {
            Ok((position, result)) => CallResponse { position, outcome: Some(call_response::Outcome::Result(result)) },
            Err(error) => CallResponse { position: None, outcome: Some(call_response::Outcome::Error(error)) },
        };
        if response.encoded_len() > MESSAGE_BYTES {
            let error = errors::invalid("the result exceeds the 16 MiB message limit");
            return Ok(Response::new(CallResponse {
                position: None,
                outcome: Some(call_response::Outcome::Error(error)),
            }));
        }
        Ok(Response::new(response))
    }

    type SubscribeStream = streams::Stream;

    async fn subscribe(&self, request: Request<SubscribeRequest>) -> Result<Response<Self::SubscribeStream>, Status> {
        let principal = self.credentials.authenticate(&request)?;
        let request = request.into_inner();
        let (sender, stream) = streams::channel();
        match self.open(principal, &request).await {
            Ok(topic) => drop(tokio::spawn(topic.run(sender, self.stop.clone()))),
            Err(error) => sender.fail(error),
        }
        Ok(Response::new(stream))
    }
}

#[cfg(test)]
mod tests;
