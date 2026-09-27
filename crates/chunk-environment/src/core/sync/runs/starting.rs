//! How a command starts: under the operation ID a subscription reserved, or one a start takes first, while calls wait
//! for it.

use super::{Pending, Phase, Run, Runs, errors};
use chunk_backend::CommandRequest;
use chunk_proto::sync::v1::{Error, error::Code};
use chunk_service::same_secret;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Calls that may wait at once for a command to start: duplicate starts, and subscriptions.
const WAITERS: usize = 4;
/// How long core refuses the start of a command whose subscription closed before it started: longer than a gateway waits
/// for a start.
const CLOSED: Duration = Duration::from_secs(60);

impl Runs {
    /// The command under `operation`, or a new one `owner` starts for `request`, cancelled through `cancel`, which the
    /// second value marks. A new start takes over the place a subscription of `owner` reserved, unless that subscription
    /// closed first, which refuses it. A finished command that only reports its outcome to a subscription doesn't know
    /// its request, so a new start checks it with the backend without taking its place.
    pub fn begin(
        &self,
        operation: &str,
        owner: &str,
        request: CommandRequest,
        cancel: CancellationToken,
    ) -> Result<(Arc<Run>, bool), Error> {
        let mut runs = self.lock();
        let current = runs.get(operation);
        if let Some(run) = current {
            let mut found = Ok(None);
            run.state.send_if_modified(|pending| {
                found = pending.adopt(same_secret(&run.owner, owner), request);
                matches!(found, Ok(Some(true)))
            });
            if let Some(new) = found? {
                return Ok((run.clone(), new));
            }
        }
        let run = Arc::new(Run::new(owner, Some(request), Phase::Starting, cancel));
        if current.is_none() {
            runs.insert(operation.to_owned(), run.clone());
        }
        Ok((run, true))
    }

    /// Waits, through the returned guard, for the command under `operation` to start: `found`, or the run in its place,
    /// or else a reservation of the unused ID for `owner`, cancelled through `cancel`. Fails if another credential
    /// started the command, or if as many calls wait for it already as may.
    pub fn reserve(
        self: &Arc<Self>,
        operation: &str,
        found: Option<Arc<Run>>,
        owner: &str,
        cancel: CancellationToken,
    ) -> Result<Awaiting, Error> {
        let mut runs = self.lock();
        let run = runs.get(operation).cloned().or(found).unwrap_or_else(|| {
            let run = Arc::new(Run::new(owner, None, Phase::Starting, cancel));
            runs.insert(operation.to_owned(), run.clone());
            run
        });
        run.permits(owner, None)?;
        let mut waiting = Ok(false);
        run.state.send_if_modified(|pending| {
            if pending.waits() {
                waiting = enter(&run.waiting).map(|()| true);
                pending.awaiting += usize::from(waiting == Ok(true));
            }
            false
        });
        let waiting = waiting?;
        Ok(Awaiting { runs: self.clone(), operation: operation.to_owned(), run, waiting })
    }

    /// Marks `run`, which was starting, rejected, and forgets it unless a subscription waits for it to start.
    pub fn reject(&self, operation: &str, run: &Arc<Run>) {
        let mut runs = self.lock();
        let mut forget = false;
        run.state.send_if_modified(|pending| {
            let starting = matches!(pending.phase, Phase::Starting);
            if starting {
                pending.phase = Phase::Rejected;
                forget = pending.awaiting == 0;
            }
            starting
        });
        if forget && runs.get(operation).is_some_and(|current| Arc::ptr_eq(current, run)) {
            runs.remove(operation);
        }
    }
}

/// Counts one more of at most [`WAITERS`] calls waiting for a command to start.
fn enter(waiting: &AtomicUsize) -> Result<(), Error> {
    let entered =
        waiting.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| (count < WAITERS).then_some(count + 1));
    entered.map(drop).map_err(|_| errors::error(Code::Unavailable, "too many calls wait for this command to start"))
}

/// Why a command whose subscription closed before it started never runs.
pub(in super::super) fn closed_before_start() -> Error {
    errors::error(Code::Stopped, "the command's subscription closed before it started")
}

/// One of at most [`WAITERS`] calls waiting for a command to start.
pub(super) struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    pub fn enter(waiting: &'a AtomicUsize) -> Result<Self, Error> {
        enter(waiting)?;
        Ok(Self(waiting))
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A subscription waiting for its command to start, as one of the calls that may. Dropped while it waits, it closes
/// the command, which cancels its start, including one waiting for admission, and refuses a later one.
pub(in super::super) struct Awaiting {
    runs: Arc<Runs>,
    operation: String,
    run: Arc<Run>,
    /// Whether the subscription still waits.
    waiting: bool,
}

impl Awaiting {
    /// Whether the command has yet to start.
    pub fn waits(&self) -> bool {
        self.waiting
    }

    /// Waits until the command started, through a later start if one is rejected, and returns it. Fails with STOPPED if
    /// another subscription waiting for it closed first.
    pub async fn started(mut self) -> Result<Arc<Run>, Error> {
        let mut state = self.run.state.subscribe();
        // The run holds the sender, so this never fails while it's borrowed.
        let _ = state.wait_for(|pending| !pending.waits()).await;
        self.leave(false);
        if self.run.closed() {
            return Err(closed_before_start());
        }
        Ok(self.run.clone())
    }

    /// Stops waiting, closing the command if `close` and it hasn't started.
    fn leave(&mut self, close: bool) {
        if !std::mem::take(&mut self.waiting) {
            return;
        }
        self.run.waiting.fetch_sub(1, Ordering::Relaxed);
        let _runs = self.runs.lock();
        let closing = self.run.state.send_if_modified(|pending| {
            pending.awaiting -= 1;
            let closing = close && pending.waits();
            if closing {
                pending.phase = Phase::Closed;
            }
            closing
        });
        if closing {
            self.run.cancel.cancel();
            let (runs, operation, run) = (self.runs.clone(), self.operation.clone(), self.run.clone());
            tokio::spawn(async move {
                tokio::time::sleep(CLOSED).await;
                runs.remove(&operation, &run);
            });
        }
    }
}

impl Drop for Awaiting {
    fn drop(&mut self) {
        self.leave(true);
    }
}

impl Pending {
    /// Whether a subscription waits for the command to start: it hasn't, and no start was rejected for good.
    fn waits(&self) -> bool {
        matches!(self.phase, Phase::Starting | Phase::Rejected)
    }

    /// Takes over this place for a start of `request` if it's a reservation whose owner is `own`, returning whether it
    /// did, or `None` if the run only reports a finished command. Fails if a subscription closed it.
    fn adopt(&mut self, own: bool, request: CommandRequest) -> Result<Option<bool>, Error> {
        let reserved = match self.phase {
            Phase::Closed => return Err(closed_before_start()),
            Phase::Starting => self.request.is_none(),
            Phase::Rejected => true,
            Phase::Running | Phase::Finished(_) if self.request.is_none() => return Ok(None),
            Phase::Running | Phase::Finished(_) => false,
        };
        if !(reserved && own) {
            return Ok(Some(false));
        }
        self.phase = Phase::Starting;
        self.request = Some(request);
        Ok(Some(true))
    }
}
