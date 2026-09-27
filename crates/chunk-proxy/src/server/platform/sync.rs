//! This gateway's one connection to core over the sync protocol, and the follower of its `gateway/<id>` topic that
//! every claim call names.

use std::{fmt, io, sync::OnceLock, time::Duration};

use chunk_proto::sync::v1::{
    CallRequest, Error, Position, call_response::Outcome, core_client::CoreClient, error::Code,
};
use prost::Message;
use tokio::sync::watch;
use tokio_util::sync::DropGuard;
use tonic::transport::Channel;

use super::{RPC_TIMEOUT, claims, claims::View, invalid_data};
use crate::GatewayCredential;

/// Core sends messages of up to 16 MiB.
const MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Flow-control windows large enough that a whole snapshot never waits on window updates. h2 derives its data frame
/// budget from the connection window.
const STREAM_WINDOW: u32 = 16 * 1024 * 1024;
const CONNECTION_WINDOW: u32 = 32 * 1024 * 1024;

pub(super) struct Connection {
    client: CoreClient<Channel>,
    gateway: GatewayCredential,
    claims: OnceLock<(watch::Receiver<View>, DropGuard)>,
}

impl Connection {
    pub fn new(endpoint: &str, gateway: GatewayCredential) -> io::Result<Self> {
        let channel = super::endpoint(endpoint)?
            .initial_stream_window_size(STREAM_WINDOW)
            .initial_connection_window_size(CONNECTION_WINDOW)
            .connect_lazy();
        let client = CoreClient::new(channel).max_decoding_message_size(MESSAGE_BYTES);
        Ok(Self { client, gateway, claims: OnceLock::new() })
    }

    pub fn gateway(&self) -> &str {
        &self.gateway.id
    }

    /// Waits for a live view of this gateway's claims in which `ready` returns a value. The first call starts
    /// following the topic, until this client drops.
    pub async fn claims<T>(&self, ready: impl FnMut(&View) -> Option<T>) -> io::Result<T> {
        let (view, _) = self.claims.get_or_init(|| claims::follow(self.client.clone(), self.gateway.clone()));
        claims::wait(view.clone(), ready).await
    }

    /// Calls platform method `chunk:<method>` on the claim `operation` names, returning its result and control's
    /// position after it. A call core stops because the stream it named was superseded runs again on the topic's next
    /// stream; any other stop fails the call. A runtime stop racing a stream change runs once more, under the same
    /// operation, and is stopped again.
    pub async fn call<R: Message + Default>(
        &self,
        method: &str,
        operation: &str,
        arguments: &impl Message,
        timeout: Duration,
    ) -> io::Result<(R, Option<Position>)> {
        let mut stale = None;
        loop {
            let stream =
                self.claims(|view| view.stream().filter(|stream| Some(*stream) != stale.as_deref()).map(str::to_owned));
            let stream = tokio::time::timeout(RPC_TIMEOUT, stream)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gateway topic unavailable"))??;
            let message = CallRequest {
                operation_id: operation.to_owned(),
                method: format!("chunk:{method}"),
                arguments: arguments.encode_to_vec(),
                stream: stream.clone(),
                ..CallRequest::default()
            };
            match self.send(message, timeout).await? {
                Ok((result, position)) => return Ok((R::decode(result.as_slice()).map_err(invalid_data)?, position)),
                Err(error) if error.code() == Code::Stopped && self.superseded(&stream) => stale = Some(stream),
                Err(error) => return Err(io::Error::other(Failure(error))),
            }
        }
    }

    /// Runs `message`, which names no stream, returning its result.
    pub async fn unfenced(&self, message: CallRequest) -> io::Result<Vec<u8>> {
        let outcome = self.send(message, RPC_TIMEOUT).await?;
        outcome.map(|(result, _)| result).map_err(|error| io::Error::other(Failure(error)))
    }

    async fn send(
        &self,
        message: CallRequest,
        timeout: Duration,
    ) -> io::Result<Result<(Vec<u8>, Option<Position>), Error>> {
        let mut request = super::authorized(message, &self.gateway.credential)?;
        request.set_timeout(timeout);
        let response = self.client.clone().call(request).await.map_err(io::Error::other)?.into_inner();
        match response.outcome {
            Some(Outcome::Result(result)) => Ok(Ok((result, response.position))),
            Some(Outcome::Error(error)) => Ok(Err(error)),
            None => Err(invalid_data("core returned no outcome")),
        }
    }

    fn superseded(&self, stream: &str) -> bool {
        self.claims.get().is_some_and(|(view, _)| view.borrow().superseded(stream))
    }
}

/// A protocol error core returned for a call.
#[derive(Debug)]
pub(in crate::server) struct Failure(pub Error);

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.message)
    }
}

impl std::error::Error for Failure {}

/// The protocol error `error` carries, if core returned one.
pub(in crate::server) fn failure(error: &io::Error) -> Option<&Error> {
    error.get_ref()?.downcast_ref::<Failure>().map(|failure| &failure.0)
}
