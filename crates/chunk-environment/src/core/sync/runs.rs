//! Commands gateways started, by operation ID, while they start and run: the gateway credential each belongs to, the
//! packet effects it holds until that gateway acknowledges them, the subscriptions following it, and its outcome.

use super::errors;
use chunk_proto::sync::v1::{CommandEffect, CommandOutcome, Entry, Error, Update, command_outcome, entry::State};
use chunk_service::same_secret;
use prost::Message;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

/// Packet effects a command may have pending at once.
const PENDING: usize = 8;
/// How long a started command waits for its first subscription, or for another once a superseded gateway stream ended
/// its last, before it's cancelled.
const GRACE: Duration = Duration::from_secs(5);

/// A command's outcome as its topic's final entry holds it.
pub(super) fn outcome(result: chunk_backend::Result<Arc<str>>) -> CommandOutcome {
    let outcome = match result {
        Ok(json) => command_outcome::Outcome::ResultJson(json.as_bytes().to_vec()),
        Err(failure) => command_outcome::Outcome::Error(errors::backend(&failure)),
    };
    CommandOutcome { outcome: Some(outcome) }
}

#[derive(Default)]
pub(super) struct Runs(Mutex<HashMap<String, Arc<Run>>>);

impl Runs {
    /// The command under `operation`, or a new one `owner` starts, cancelled through `cancel`, which the second value
    /// marks.
    pub fn begin(&self, operation: &str, owner: &str, cancel: CancellationToken) -> (Arc<Run>, bool) {
        let mut runs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(run) = runs.get(operation) {
            return (run.clone(), false);
        }
        let run = Arc::new(Run::new(owner, Phase::Starting, cancel));
        runs.insert(operation.to_owned(), run.clone());
        (run, true)
    }

    pub fn get(&self, operation: &str) -> Option<Arc<Run>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).get(operation).cloned()
    }

    /// Forgets `run`, which finished or never started.
    pub fn remove(&self, operation: &str, run: &Arc<Run>) {
        let mut runs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if runs.get(operation).is_some_and(|current| Arc::ptr_eq(current, run)) {
            runs.remove(operation);
        }
    }
}

pub(super) struct Run {
    /// The gateway credential whose `chunk:command` started the command.
    owner: String,
    state: watch::Sender<Pending>,
    /// Cancels the command's work: its handler and the effects core performs for it.
    cancel: CancellationToken,
}

enum Phase {
    Starting,
    Rejected,
    Running,
    Finished(CommandOutcome),
}

pub(super) struct Pending {
    phase: Phase,
    effects: BTreeMap<u32, Held>,
    subscribers: usize,
    /// When the running command is cancelled unless a subscription opens first.
    orphaned: Option<Instant>,
}

struct Held {
    value: CommandEffect,
    effect: chunk_backend::CommandEffect,
}

impl Run {
    fn new(owner: &str, phase: Phase, cancel: CancellationToken) -> Self {
        let pending = Pending { phase, effects: BTreeMap::new(), subscribers: 0, orphaned: None };
        Self { owner: owner.to_owned(), state: watch::Sender::new(pending), cancel }
    }

    /// A command of `owner` that finished with `outcome` and whose run is gone, for a subscription to report.
    pub fn finished(owner: &str, outcome: CommandOutcome) -> Arc<Self> {
        Arc::new(Self::new(owner, Phase::Finished(outcome), CancellationToken::new()))
    }

    /// Checks that `credential` started the command.
    pub fn permits(&self, credential: &str) -> Result<(), Error> {
        if same_secret(&self.owner, credential) {
            Ok(())
        } else {
            Err(errors::denied("another gateway ran this command"))
        }
    }

    /// Waits until the command started or was rejected, returning whether it started.
    pub async fn started(&self) -> bool {
        let mut state = self.state.subscribe();
        let phase = state.wait_for(|pending| !matches!(pending.phase, Phase::Starting)).await;
        phase.is_ok_and(|pending| !matches!(pending.phase, Phase::Rejected))
    }

    /// Marks the command started, which waits [`GRACE`] for its first subscription.
    pub fn start(&self) {
        self.state.send_modify(|pending| {
            pending.phase = Phase::Running;
            pending.orphaned = (pending.subscribers == 0).then(|| Instant::now() + GRACE);
        });
    }

    /// Marks the command rejected before it started.
    pub fn reject(&self) {
        self.state.send_modify(|pending| pending.phase = Phase::Rejected);
    }

    /// Follows the command until the returned subscription drops.
    pub fn follow(self: &Arc<Self>) -> (watch::Receiver<Pending>, Subscription) {
        self.state.send_modify(|pending| {
            pending.subscribers += 1;
            pending.orphaned = None;
        });
        (self.state.subscribe(), Subscription { run: self.clone(), superseded: false })
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

    /// Holds `effect` for the gateway to render as `value`, or fails it once the command was cancelled, finished or
    /// has as many pending as it may.
    pub fn publish(&self, effect: chunk_backend::CommandEffect, value: CommandEffect) {
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
            pending.effects.insert(effect.sequence(), Held { value, effect });
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
            held = self.permits(credential).map(|()| pending.effects.remove(&sequence));
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
    pub fn finish(&self, outcome: CommandOutcome) {
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

/// A subscription following a command, which the command may be cancelled without once it drops.
pub(super) struct Subscription {
    run: Arc<Run>,
    superseded: bool,
}

impl Subscription {
    /// Marks the subscription as ending because its gateway stream was superseded, which leaves the command
    /// [`GRACE`] for the gateway to follow it again.
    pub fn superseded(&mut self) {
        self.superseded = true;
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let grace = if self.superseded { GRACE } else { Duration::ZERO };
        self.run.state.send_modify(|pending| {
            pending.subscribers -= 1;
            if pending.subscribers == 0 && matches!(pending.phase, Phase::Running) {
                pending.orphaned = Some(Instant::now() + grace);
            }
        });
    }
}

impl Pending {
    /// The pending effects as a snapshot, or once the command finished, its outcome, which the second value marks.
    pub fn snapshot(&self) -> (Update, bool) {
        let entry = |key: String, value: Vec<u8>| Entry { key, state: Some(State::Value(value)) };
        let (upserts, finished) = if let Phase::Finished(outcome) = &self.phase {
            (vec![entry("outcome".into(), outcome.encode_to_vec())], true)
        } else {
            let effects = self.effects.iter();
            (effects.map(|(sequence, held)| entry(sequence.to_string(), held.value.encode_to_vec())).collect(), false)
        };
        (Update { snapshot: true, upserts, ..Update::default() }, finished)
    }
}
