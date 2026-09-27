//! This gateway's one connection to core over the sync protocol, and the follower of its `gateway/<id>` topic that
//! every claim call names.

use std::{fmt, io, sync::OnceLock, time::Duration};

use chunk_proto::sync::v1::{
    CallRequest, Error, Position, SubscribeRequest, Update, call_response::Outcome, core_client::CoreClient,
    error::Code,
};
use tokio::sync::watch;
use tokio_util::sync::DropGuard;
use tonic::{Streaming, transport::Channel};

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

    /// Runs `message` on the gateway's current stream, returning its result and control's position after it. A call
    /// core stops because the stream it named was superseded runs again on the topic's next stream; any other stop
    /// fails the call. A runtime stop racing a stream change runs once more, under the same operation, and is stopped
    /// again.
    pub async fn fenced(&self, mut message: CallRequest, timeout: Duration) -> io::Result<(Vec<u8>, Option<Position>)> {
        let mut stale = None;
        loop {
            message.stream = self.stream(stale.as_deref()).await?;
            match self.send(message.clone(), timeout).await? {
                Ok(result) => return Ok(result),
                Err(error) if error.code() == Code::Stopped && self.superseded(&message.stream) => {
                    stale = Some(std::mem::take(&mut message.stream));
                }
                Err(error) => return Err(io::Error::other(Failure(error))),
            }
        }
    }

    /// The gateway's current `gateway/<id>` stream once its view is live, other than `stale`.
    pub async fn stream(&self, stale: Option<&str>) -> io::Result<String> {
        let stream = self.claims(|view| view.stream().filter(|stream| Some(*stream) != stale).map(str::to_owned));
        tokio::time::timeout(RPC_TIMEOUT, stream)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gateway topic unavailable"))?
    }

    /// Subscribes to the topic `request` names.
    pub async fn subscribe(&self, request: SubscribeRequest) -> io::Result<Streaming<Update>> {
        let request = super::authorized(request, &self.gateway.credential)?;
        Ok(self.client.clone().subscribe(request).await.map_err(io::Error::other)?.into_inner())
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

    /// Whether `stream` was superseded, as the claims follower left it.
    pub fn superseded(&self, stream: &str) -> bool {
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
