//! Runs hosts' machines through management's capacity API.

use crate::{LaunchSpec, Launcher};
use chunk_management::{Client, Code, Error, Status, v1};
use std::{io, time::Duration};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// How long a launch waits before asking management again about a machine it is provisioning.
const POLL: Duration = Duration::from_secs(1);

/// Asks management for each host's JVM machine, with the host ID as the capacity request's ID, and releases it by that
/// ID. Management fences both calls by the lease of core's latest attach, and keeps a released request released, which
/// fences its ID. Management tells machines where core is, so `LaunchSpec::core_endpoint` is unused.
pub(crate) struct ManagementLauncher {
    client: Client,
    /// The lease of core's latest attach, kept once that attach ends.
    lease: watch::Receiver<Option<u64>>,
}

impl ManagementLauncher {
    pub(crate) fn new(client: Client, lease: watch::Receiver<Option<u64>>) -> Self {
        Self { client, lease }
    }

    /// The latest lease, once core has attached.
    async fn lease(&self) -> io::Result<u64> {
        let mut lease = self.lease.clone();
        let latest = lease.wait_for(Option::is_some).await.map_err(|_| io::Error::other("core never attached"))?;
        (*latest).ok_or_else(|| io::Error::other("core never attached"))
    }

    /// Whether `error` may pass when the same request is sent again: it failed in transit or for a while, or under a
    /// lease a newer attach has replaced.
    fn retryable(&self, error: &Error, lease: u64) -> bool {
        match error.code() {
            Code::Unavailable
            | Code::DeadlineExceeded
            | Code::ResourceExhausted
            | Code::Aborted
            | Code::Internal
            | Code::Unknown => true,
            Code::FailedPrecondition => *self.lease.borrow() != Some(lease),
            _ => false,
        }
    }
}

#[tonic::async_trait]
impl Launcher for ManagementLauncher {
    /// Ensures the capacity until management reports it ready or failed, sending the same request again after errors
    /// that may pass. `cancel` never interrupts a call, only the waits between calls, so a release core sends once the
    /// launch returned follows every call management answered.
    async fn launch(
        &self,
        id: &str,
        credential: &str,
        spec: &LaunchSpec,
        cancel: &CancellationToken,
    ) -> io::Result<()> {
        let mut request = v1::EnsureCapacityRequest {
            request_id: id.to_owned(),
            workload: v1::Workload::Jvm.into(),
            machine_profile: spec.profile.clone(),
            release_id: spec.release_id.clone(),
            app_id: spec.app.clone(),
            lease: 0,
            credential: credential.to_owned(),
        };
        loop {
            request.lease = tokio::select! {
                lease = self.lease() => lease?,
                () = cancel.cancelled() => return Err(cancelled()),
            };
            let ensured = bounded(self.client.ensure_capacity(&request)).await;
            match ensured.map(|response| response.capacity.unwrap_or_default()) {
                Ok(capacity) if capacity.state() == v1::CapacityState::Provisioning => {}
                Ok(capacity) => return ready(&capacity),
                Err(error) if self.retryable(&error, request.lease) => {
                    tracing::warn!(host = id, %error, "ensuring the machine's capacity failed; trying again");
                }
                Err(error) => return Err(io::Error::other(error)),
            }
            tokio::select! {
                () = tokio::time::sleep(POLL) => {}
                () = cancel.cancelled() => return Err(cancelled()),
            }
        }
    }

    async fn release(&self, id: &str) -> io::Result<bool> {
        let request = v1::ReleaseCapacityRequest { request_id: id.to_owned(), lease: self.lease().await? };
        let capacity = bounded(self.client.release_capacity(&request)).await.map_err(io::Error::other)?.capacity;
        match capacity.unwrap_or_default().state() {
            v1::CapacityState::Released => Ok(true),
            v1::CapacityState::Releasing => Ok(false),
            state => Err(io::Error::other(format!("the released capacity request is {}", state.as_str_name()))),
        }
    }
}

/// How a request management no longer provisions ended.
fn ready(capacity: &v1::Capacity) -> io::Result<()> {
    match capacity.state() {
        v1::CapacityState::Ready => Ok(()),
        v1::CapacityState::Failed => {
            Err(io::Error::other(format!("management failed the machine: {}", capacity.message)))
        }
        state => Err(io::Error::other(format!("the capacity request is {}", state.as_str_name()))),
    }
}

/// `call`'s result, or a deadline error once management took too long.
async fn bounded<T>(call: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    tokio::time::timeout(super::REQUEST_TIMEOUT, call).await.unwrap_or_else(|_| {
        let message = format!("management did not answer within {}s", super::REQUEST_TIMEOUT.as_secs());
        Err(Status { code: Code::DeadlineExceeded, message }.into())
    })
}

fn cancelled() -> io::Error {
    io::Error::other("the launch was cancelled")
}
