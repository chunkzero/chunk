//! Hands the backend's next due job to management as the environment's wake alarm, so a suspended environment starts
//! again in time for it.

use super::{Interrupted, REQUEST_TIMEOUT, deadline, status::OBSERVE};
use chunk_management::{Client, v1};
use chunk_store::WakeHandoff;
use std::{
    io,
    sync::{Mutex, PoisonError},
    time::{Duration, SystemTime},
};
use tokio::time::MissedTickBehavior;

/// An alarm management stored and the backend acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Handed {
    epoch: u64,
    generation: u64,
    due_at: Option<i64>,
}

pub(super) struct Alarm {
    client: Client,
    /// The latest alarm this process handed off. The backend's acknowledgement outlives a restore to a new epoch, whose
    /// alarm management has not seen, so each process hands its alarm off at least once.
    handed: Mutex<Option<Handed>>,
}

impl Alarm {
    pub(super) fn new(client: Client) -> Self {
        Self { client, handed: Mutex::default() }
    }

    /// Whether `handoff`, from the backend serving `epoch`, is the alarm this process handed off.
    pub(super) fn settled(&self, epoch: u64, handoff: &WakeHandoff) -> bool {
        let current = Handed { epoch, generation: handoff.generation, due_at: handoff.due_at };
        handoff.acknowledged && *self.lock() == Some(current)
    }

    /// Hands off each new alarm of `backend`, which serves `epoch`, under the lease `lease` finds, checking every
    /// [`OBSERVE`]. Nothing is handed off while `lease` finds none. A failed handoff is read again and retried.
    pub(super) async fn keep_handing_off(
        &self,
        backend: &chunk_backend::Backend,
        epoch: u64,
        lease: impl Fn() -> Option<u64>,
    ) -> ! {
        let mut tick = tokio::time::interval(OBSERVE);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(lease) = lease() else { continue };
            match self.hand_off(backend, epoch, lease).await {
                Ok(()) => {}
                Err(Interrupted::Fenced(error)) => tracing::debug!(%error, "wake alarm fenced; reading it again"),
                Err(Interrupted::Retry(error) | Interrupted::Fatal(error)) => {
                    tracing::warn!(%error, "wake alarm handoff failed; retrying");
                }
            }
        }
    }

    /// Stores the backend's current alarm with management under `lease`, then acknowledges it to the backend. It
    /// acknowledges only once management echoes the alarm it was sent: a newer one stored there, or a backend whose
    /// alarm moved meanwhile, leaves it for the next read.
    async fn hand_off(&self, backend: &chunk_backend::Backend, epoch: u64, lease: u64) -> Result<(), Interrupted> {
        let handoff = backend.wake_handoff().await.map_err(backend_error)?;
        let wanted = Handed { epoch, generation: handoff.generation, due_at: handoff.due_at };
        if handoff.acknowledged && *self.lock() == Some(wanted) {
            return Ok(());
        }
        let request = v1::SetWakeAlarmRequest {
            generation: wanted.generation,
            due_time: wanted.due_at.map(|millis| {
                (SystemTime::UNIX_EPOCH + Duration::from_millis(u64::try_from(millis).unwrap_or(0))).into()
            }),
            epoch,
            lease,
        };
        let stored = deadline(REQUEST_TIMEOUT, self.client.set_wake_alarm(&request)).await?;
        if (stored.epoch, stored.generation, stored.due_time) != (request.epoch, request.generation, request.due_time) {
            return Err(Interrupted::Retry(io::Error::other(format!(
                "management holds a newer wake alarm (epoch {}, generation {}) than generation {} of epoch {}",
                stored.epoch, stored.generation, request.generation, request.epoch
            ))));
        }
        backend.acknowledge_wake(wanted.generation, wanted.due_at).await.map_err(backend_error)?;
        *self.lock() = Some(wanted);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Handed>> {
        self.handed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn backend_error(error: chunk_backend::Error) -> Interrupted {
    Interrupted::Retry(io::Error::other(error))
}
