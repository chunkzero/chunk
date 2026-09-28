//! How a command starts: under the operation ID a subscription reserved, or one a start takes first, while calls and
//! the subscription following it wait for it.

use super::{Pending, Phase, Run, Runs, errors};
use chunk_backend::CommandRequest;
use chunk_proto::sync::v1::{Error, error::Code};
use chunk_service::same_secret;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

/// Calls that may wait at once for a command to start: duplicate starts, and subscriptions.
const WAITERS: usize = 4;

impl Runs {
    /// The command under `operation`, or a new one `owner` starts for `request`, cancelled through `cancel`, which the
    /// second value marks. A new start takes over the place a subscription of `owner` reserved, unless that subscription
    /// closed first, which refuses it, or an earlier start there asked for another request. A finished command that
    /// only reports its outcome to a subscription doesn't know its request, so a new start checks it with the backend
    /// without taking its place.
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

    /// Marks `run`, which was starting, rejected, and forgets it unless a subscription follows it.
    pub fn reject(&self, operation: &str, run: &Arc<Run>) {
        let mut runs = self.lock();
        let mut forget = false;
        run.state.send_if_modified(|pending| {
            let starting = matches!(pending.phase, Phase::Starting);
            if starting {
                pending.phase = Phase::Rejected;
                forget = pending.following.is_none();
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

impl Pending {
    /// Whether the command has yet to start: it hasn't, and no start was rejected for good.
    pub(in super::super) fn waits(&self) -> bool {
        matches!(self.phase, Phase::Starting | Phase::Rejected)
    }

    /// Makes a new subscription the one following the command, returning its number and whether it waits for the
    /// command to start, as one of the calls in `waiting` that may. Fails once a subscription closed the command.
    pub(super) fn follow(&mut self, waiting: &AtomicUsize) -> Result<(u64, bool), Error> {
        if matches!(self.phase, Phase::Closed) {
            return Err(closed_before_start());
        }
        let waits = self.waits();
        if waits {
            enter(waiting)?;
        }
        self.opened += 1;
        self.following = Some(self.opened);
        self.orphaned = None;
        Ok((self.opened, waits))
    }

    /// Takes over this place for a start of `request` if it's a reservation whose owner is `own` and whose first start,
    /// if any, asked for `request`, returning whether it did, or `None` if the run only reports a finished command.
    /// Fails if a subscription closed it.
    fn adopt(&mut self, own: bool, request: CommandRequest) -> Result<Option<bool>, Error> {
        let reserved = match self.phase {
            Phase::Closed => return Err(closed_before_start()),
            Phase::Starting => self.request.is_none(),
            Phase::Rejected => self.request == Some(request),
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
