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
    /// position after it. A call naming a superseded stream runs again on the topic's next stream.
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
            let mut request = super::authorized(message, &self.gateway.credential)?;
            request.set_timeout(timeout);
            let response = self.client.clone().call(request).await.map_err(io::Error::other)?.into_inner();
            match response.outcome {
                Some(Outcome::Result(result)) => {
                    return Ok((R::decode(result.as_slice()).map_err(invalid_data)?, response.position));
                }
                Some(Outcome::Error(error)) if error.code() == Code::Stopped => stale = Some(stream),
                Some(Outcome::Error(error)) => return Err(io::Error::other(Failure(error))),
                None => return Err(invalid_data("core returned no outcome")),
            }
        }
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
