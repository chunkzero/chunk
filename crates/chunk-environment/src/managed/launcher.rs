//! Runs hosts' machines through management's capacity API.

use crate::{LaunchSpec, Launcher};
use chunk_management::{Client, Code, Error, Status, v1};
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// How long a launch waits before asking management again about a machine it is provisioning.
const POLL: Duration = Duration::from_secs(1);

/// This core's hold on the environment, as `Managed` publishes it from each attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lease {
    /// Core has not attached yet.
    Waiting,
    /// The lease of core's latest attach, kept once that attach ends.
    Held(u64),
    /// Another core superseded this one. It never changes back.
    Superseded,
}

/// Asks management for each host's JVM machine, with the host ID as the capacity request's ID, and releases it by that
/// ID. Management fences both calls by the lease of core's latest attach, and keeps a released request released, which
/// fences its ID. Once another core supersedes this one, management releases every request this core created, so a
/// superseded core's releases are confirmed at once. Management tells machines where core is, so
/// `LaunchSpec::core_endpoint` is unused.
pub(crate) struct ManagementLauncher {
    client: Client,
    lease: watch::Receiver<Lease>,
    /// Whether later releases are confirmed without asking management.
    abandoned: AtomicBool,
}

impl ManagementLauncher {
    pub(crate) fn new(client: Client, lease: watch::Receiver<Lease>) -> Self {
        Self { client, lease, abandoned: AtomicBool::new(false) }
    }

    /// Confirms every later release at once, as when core exits with machine stops unconfirmed: the next core to
    /// attach supersedes this one, and management then releases its machines.
    pub(crate) fn abandon(&self) {
        self.abandoned.store(true, Ordering::Relaxed);
    }

    /// The lease to call under once core holds one other than `stale`, which management rejected. `None` once no such
    /// lease can come: another core superseded this one, or `Managed` stopped, so nothing publishes a newer one.
    async fn lease(&self, stale: Option<u64>) -> Option<u64> {
        let mut lease = self.lease.clone();
        let current = lease.wait_for(|lease| match lease {
            Lease::Waiting => false,
            Lease::Held(held) => Some(*held) != stale,
            Lease::Superseded => true,
        });
        match current.await.as_deref() {
            Ok(Lease::Held(held)) => Some(*held),
            _ => None,
        }
    }
}

#[tonic::async_trait]
impl Launcher for ManagementLauncher {
    /// Ensures the capacity until management reports it ready or failed. It sends the same request again after errors
    /// that may pass, and under the next lease after management rejected the lease it carried, as during a re-attach.
    /// `cancel` never interrupts a call, only the waits between calls, so a release core sends once the launch returned
    /// follows every call management answered.
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
        let mut rejected = None;
        loop {
            let lease = tokio::select! {
                lease = self.lease(rejected.take()) => lease,
                () = cancel.cancelled() => return Err(cancelled()),
            };
            request.lease = lease.ok_or_else(|| io::Error::other("core no longer holds the environment"))?;
            let ensured = bounded(self.client.ensure_capacity(&request)).await;
            match ensured.map(|response| response.capacity.unwrap_or_default()) {
                Ok(capacity) if capacity.state() == v1::CapacityState::Provisioning => {}
                Ok(capacity) => return ready(&capacity),
                Err(error) if error.code() == Code::FailedPrecondition => {
                    rejected = Some(request.lease);
                    continue;
                }
                Err(error) if transient(&error) => {
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
        let mut rejected = None;
        loop {
            if self.abandoned.load(Ordering::Relaxed) {
                tracing::warn!(
                    host = id,
                    "the machine's release is unconfirmed; management releases it once another core attaches"
                );
                return Ok(true);
            }
            let Some(lease) = self.lease(rejected).await else { return Ok(true) };
            let request = v1::ReleaseCapacityRequest { request_id: id.to_owned(), lease };
            match bounded(self.client.release_capacity(&request)).await {
                Ok(response) => {
                    return match response.capacity.unwrap_or_default().state() {
                        v1::CapacityState::Released => Ok(true),
                        v1::CapacityState::Releasing => Ok(false),
                        state => {
                            Err(io::Error::other(format!("the released capacity request is {}", state.as_str_name())))
                        }
                    };
                }
                Err(error) if error.code() == Code::FailedPrecondition => rejected = Some(lease),
                Err(error) => return Err(io::Error::other(error)),
            }
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

/// Whether a call may have failed only in transit or for a while, so the same request may pass.
fn transient(error: &Error) -> bool {
    matches!(
        error.code(),
        Code::Unavailable
            | Code::DeadlineExceeded
            | Code::ResourceExhausted
            | Code::Aborted
            | Code::Internal
            | Code::Unknown
    )
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
