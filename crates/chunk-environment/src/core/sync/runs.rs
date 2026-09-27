//! Commands gateways started, by operation ID, while they start and run and while a subscription follows them once
//! finished: the gateway credential each belongs to and the request it started, the packet effects it holds until that
//! gateway acknowledges them, the one subscription following it, and its outcome. Held effects and outcomes are charged
//! against the backend's request memory until they drop.

use super::errors;
use chunk_backend::{Backend, CommandRequest, RequestCharge};
use chunk_proto::sync::v1::{
    CommandEffect, CommandOutcome, Entry, Error, Update, command_outcome, entry::State, error::Code,
};
use chunk_service::same_secret;
use prost::Message;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

/// Packet effects a command may have pending at once.
const PENDING: usize = 8;
/// Calls that may wait at once for a command to start: duplicate starts, and subscriptions that raced its start.
const WAITERS: usize = 4;
/// How long a started command waits for its first subscription, or for another once a superseded gateway stream ended
/// its last, before it's cancelled. Any other end of its subscription cancels it at once.
const GRACE: Duration = Duration::from_secs(5);

/// A command's outcome as its topic's final entry holds it, charged for as long as core holds it.
pub(super) struct Outcome {
    value: CommandOutcome,
    _charge: RequestCharge,
}

/// The outcome `result` names, or why `backend` has no room for core to hold it.
pub(super) fn outcome(backend: &Backend, result: chunk_backend::Result<Arc<str>>) -> Result<Outcome, Error> {
    let outcome = match result {
        Ok(json) => command_outcome::Outcome::ResultJson(json.as_bytes().to_vec()),
        Err(failure) => command_outcome::Outcome::Error(errors::backend(&failure)),
    };
    let value = CommandOutcome { outcome: Some(outcome) };
    let charge = backend.charge_request(value.encoded_len()).map_err(|failure| errors::backend(&failure))?;
    Ok(Outcome { value, _charge: charge })
}

#[derive(Default)]
pub(super) struct Runs(Mutex<HashMap<String, Arc<Run>>>);

impl Runs {
    /// The command under `operation`, or a new one `owner` starts for `request`, cancelled through `cancel`, which the
    /// second value marks. A finished command that only reports its outcome to a subscription doesn't know its request,
    /// so a new start checks it with the backend without taking its place.
    pub fn begin(
        &self,
        operation: &str,
        owner: &str,
        request: CommandRequest,
        cancel: CancellationToken,
    ) -> (Arc<Run>, bool) {
        let mut runs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let current = runs.get(operation);
        if let Some(run) = current.filter(|run| run.request.is_some()) {
            return (run.clone(), false);
        }
        let run = Arc::new(Run::new(owner, Some(request), Phase::Starting, cancel));
        if current.is_none() {
            runs.insert(operation.to_owned(), run.clone());
        }
        (run, true)
    }

    pub fn get(&self, operation: &str) -> Option<Arc<Run>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).get(operation).cloned()
    }

    /// Forgets `run`, which never started.
    pub fn remove(&self, operation: &str, run: &Arc<Run>) {
        let mut runs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if runs.get(operation).is_some_and(|current| Arc::ptr_eq(current, run)) {
            runs.remove(operation);
        }
    }

    /// Forgets `run` once it finished and no subscription follows it.
    pub fn settle(&self, operation: &str, run: &Arc<Run>) {
        let mut runs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let settled = |pending: &Pending| matches!(pending.phase, Phase::Finished(_)) && pending.following.is_none();
        if runs.get(operation).is_some_and(|current| Arc::ptr_eq(current, run) && settled(&run.state.borrow())) {
            runs.remove(operation);
        }
    }

    /// Follows the command under `operation` for `credential` until the returned subscription drops or a newer one
    /// supersedes it. `run`, which started, follows it unless another run holds the operation's place; a finished `run`
    /// holds the place while followed, so every subscription to a command supersedes the one before. Fails once
    /// `superseded` is cancelled, as when the gateway stream the subscription names was superseded while it waited.
    pub fn follow(
        self: &Arc<Self>,
        operation: &str,
        run: Arc<Run>,
        credential: &str,
        superseded: &CancellationToken,
    ) -> Result<(watch::Receiver<Pending>, Subscription), Error> {
        let mut runs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let current = runs.get(operation).cloned();
        let run = current.clone().unwrap_or(run);
        run.permits(credential, None)?;
        if superseded.is_cancelled() {
            return Err(errors::error(Code::Stopped, "a newer gateway stream superseded the one named"));
        }
        let mut id = None;
        run.state.send_if_modified(|pending| {
            if matches!(pending.phase, Phase::Starting) {
                return false;
            }
            pending.opened += 1;
            id = Some(pending.opened);
            pending.following = id;
            pending.orphaned = None;
            true
        });
        let id = id.ok_or_else(|| errors::error(Code::Unavailable, "retry the command"))?;
        if current.is_none() {
            runs.insert(operation.to_owned(), run.clone());
        }
        drop(runs);
        let subscription = Subscription { runs: self.clone(), operation: operation.to_owned(), run: run.clone(), id };
        Ok((run.state.subscribe(), subscription))
    }
}

pub(super) struct Run {
    /// The gateway credential whose `chunk:command` started the command.
    owner: String,
    /// What that `chunk:command` asked for, unless the run only reports a finished command.
    request: Option<CommandRequest>,
    state: watch::Sender<Pending>,
    /// Calls waiting for the command to start.
    waiting: AtomicUsize,
    /// Cancels the command's work: its handler and the effects core performs for it.
    cancel: CancellationToken,
}

enum Phase {
    Starting,
    Rejected,
    Running,
    /// Finished, with its outcome, or why core couldn't hold it.
    Finished(Result<Outcome, Error>),
}

pub(super) struct Pending {
    phase: Phase,
    effects: BTreeMap<u32, Held>,
    /// The subscription following the command, numbered by `opened`; a newer one supersedes it.
    following: Option<u64>,
    opened: u64,
    /// When the running command is cancelled unless a subscription opens first.
    orphaned: Option<Instant>,
}

struct Held {
    value: CommandEffect,
    effect: chunk_backend::CommandEffect,
    _charge: RequestCharge,
}

impl Run {
    fn new(owner: &str, request: Option<CommandRequest>, phase: Phase, cancel: CancellationToken) -> Self {
        let pending = Pending { phase, effects: BTreeMap::new(), following: None, opened: 0, orphaned: None };
        Self {
            owner: owner.to_owned(),
            request,
            state: watch::Sender::new(pending),
            waiting: AtomicUsize::new(0),
            cancel,
        }
    }

    /// A command of `owner` that finished with `outcome` and whose run is gone, for a subscription to report.
    pub fn finished(owner: &str, outcome: Outcome) -> Arc<Self> {
        Arc::new(Self::new(owner, None, Phase::Finished(Ok(outcome)), CancellationToken::new()))
    }

    /// Checks that `credential` started the command, and for `request`, if given.
    pub fn permits(&self, credential: &str, request: Option<CommandRequest>) -> Result<(), Error> {
        if !same_secret(&self.owner, credential) {
            return Err(errors::denied("another gateway ran this command"));
        }
        if request.is_some_and(|request| self.request != Some(request)) {
            return Err(errors::error(Code::OperationMismatch, "the operation ID started another command"));
        }
        Ok(())
    }

    /// Waits until the command started or was rejected, returning whether it started, or UNAVAILABLE if as many calls
    /// wait for it already as may.
    pub async fn started(&self) -> Result<bool, Error> {
        let mut state = self.state.subscribe();
        let starting = |pending: &Pending| matches!(pending.phase, Phase::Starting);
        let _waiting = if starting(&state.borrow()) { Some(Waiting::enter(&self.waiting)?) } else { None };
        let phase = state.wait_for(|pending| !starting(pending)).await;
        Ok(phase.is_ok_and(|pending| !matches!(pending.phase, Phase::Rejected)))
    }

    /// Marks the command started, which waits [`GRACE`] for its first subscription.
    pub fn start(&self) {
        self.state.send_modify(|pending| {
            pending.phase = Phase::Running;
            pending.orphaned = pending.following.is_none().then(|| Instant::now() + GRACE);
        });
    }

    /// Marks the command rejected before it started.
    pub fn reject(&self) {
        self.state.send_modify(|pending| pending.phase = Phase::Rejected);
    }

    /// Resolves once the running command has had no subscription for as long as it may.
    pub async fn abandoned(&self) {
        let mut state = self.state.subscribe();
        loop {
            let orphaned = state.borrow_and_update().orphaned;
            let expired = async {
                match orphaned {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = expired => return,
                // The run holds the sender, so this never fails while it's borrowed.
                _ = state.changed() => {}
            }
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Holds `effect` for the gateway to render as `value`, charged by `charge`, or fails it once the command was
    /// cancelled, finished or has as many pending as it may.
    pub fn publish(&self, effect: chunk_backend::CommandEffect, value: CommandEffect, charge: RequestCharge) {
        let mut effect = Some(effect);
        self.state.send_if_modified(|pending| {
            pending.effects.retain(|_, held| !held.effect.is_cancelled());
            if self.cancel.is_cancelled()
                || !matches!(pending.phase, Phase::Running)
                || pending.effects.len() >= PENDING
            {
                return false;
            }
            let Some(effect) = effect.take() else { return false };
            pending.effects.insert(effect.sequence(), Held { value, effect, _charge: charge });
            true
        });
        if let Some(effect) = effect {
            effect.finish(None);
        }
    }

    /// Resolves pending effect `sequence` with `credential`'s acknowledgment, returning whether it was pending.
    pub fn acknowledge(&self, credential: &str, sequence: u32, failed: bool) -> Result<bool, Error> {
        let mut held = Ok(None);
        self.state.send_if_modified(|pending| {
            held = self.permits(credential, None).map(|()| pending.effects.remove(&sequence));
            matches!(held, Ok(Some(_)))
        });
        let Some(Held { effect, .. }) = held? else { return Ok(false) };
        if failed {
            effect.finish(None);
        } else {
            effect.accept();
        }
        Ok(true)
    }

    /// Marks the command finished with `outcome`, failing the effects still pending.
    pub fn finish(&self, outcome: Result<Outcome, Error>) {
        let mut effects = BTreeMap::new();
        self.state.send_modify(|pending| {
            pending.phase = Phase::Finished(outcome);
            pending.orphaned = None;
            effects = std::mem::take(&mut pending.effects);
        });
        for held in effects.into_values() {
            held.effect.finish(None);
        }
    }
}

/// One of at most [`WAITERS`] calls waiting for a command to start.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn enter(waiting: &'a AtomicUsize) -> Result<Self, Error> {
        let entered =
            waiting.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| (count < WAITERS).then_some(count + 1));
        entered.map_err(|_| errors::error(Code::Unavailable, "too many calls wait for this command to start"))?;
        Ok(Self(waiting))
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The subscription following a command. Once it drops, the running command is cancelled, and the finished one
/// leaves the runs.
pub(super) struct Subscription {
    runs: Arc<Runs>,
    operation: String,
    run: Arc<Run>,
    id: u64,
}

impl Subscription {
    /// Whether a newer subscription follows the command instead.
    pub fn replaced(&self, pending: &Pending) -> bool {
        pending.following != Some(self.id)
    }

    /// Ends the subscription because its gateway stream was superseded, which leaves the running command [`GRACE`] for
    /// the gateway to follow it again.
    pub fn superseded(self) {
        let grace = Instant::now() + GRACE;
        self.run.state.send_if_modified(|pending| {
            let running = pending.following == Some(self.id) && matches!(pending.phase, Phase::Running);
            if running {
                pending.following = None;
                pending.orphaned = Some(grace);
            }
            running
        });
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let (mut cancel, mut finished) = (false, false);
        self.run.state.send_if_modified(|pending| {
            if pending.following != Some(self.id) {
                return false;
            }
            pending.following = None;
            cancel = matches!(pending.phase, Phase::Running);
            finished = matches!(pending.phase, Phase::Finished(_));
            true
        });
        if cancel {
            self.run.cancel.cancel();
        }
        if finished {
            self.runs.settle(&self.operation, &self.run);
        }
    }
}

impl Pending {
    /// The pending effects as a snapshot, or once the command finished, its outcome, which the second value marks.
    /// Fails if core couldn't hold the outcome.
    pub fn snapshot(&self) -> Result<(Update, bool), Error> {
        let entry = |key: String, value: Vec<u8>| Entry { key, state: Some(State::Value(value)) };
        let (upserts, finished) = if let Phase::Finished(outcome) = &self.phase {
            let outcome = outcome.as_ref().map_err(Clone::clone)?;
            (vec![entry("outcome".into(), outcome.value.encode_to_vec())], true)
        } else {
            let effects = self.effects.iter();
            (effects.map(|(sequence, held)| entry(sequence.to_string(), held.value.encode_to_vec())).collect(), false)
        };
        Ok((Update { snapshot: true, upserts, ..Update::default() }, finished))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_subscription_cancels_its_command_even_once_another_follows() {
        let (runs, operation, stream) = (Arc::new(Runs::default()), "prep:a:1", CancellationToken::new());
        let request = CommandRequest::new("say", "say wait", "player");
        let (run, _) = runs.begin(operation, "gateway", request, CancellationToken::new());
        run.start();
        let (_, first) = runs.follow(operation, run.clone(), "gateway", &stream).unwrap();
        drop(first);
        let (_, _second) = runs.follow(operation, run.clone(), "gateway", &stream).unwrap();
        assert!(run.token().is_cancelled());
    }
}
