//! Running commands, by operation ID: the packet effects each holds for its gateway until the gateway acknowledges them.

use super::errors;
use chunk_proto::sync::v1::{CommandEffect, Entry, Error, Update, entry::State};
use chunk_service::same_secret;
use prost::Message;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, PoisonError, Weak},
};
use tokio::sync::watch;

/// Packet effects a command may have pending at once.
const PENDING: usize = 8;

/// Each command a stream follows or a call runs, open while either holds it.
#[derive(Default)]
pub(super) struct Runs(Mutex<HashMap<String, Weak<Run>>>);

impl Runs {
    /// The command under `operation`, opened if nothing holds it.
    pub fn open(&self, operation: &str) -> Arc<Run> {
        let mut runs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        runs.retain(|_, run| run.strong_count() > 0);
        if let Some(run) = runs.get(operation).and_then(Weak::upgrade) {
            return run;
        }
        let run = Arc::new(Run { state: watch::Sender::new(Pending::default()) });
        runs.insert(operation.to_owned(), Arc::downgrade(&run));
        run
    }

    /// The open command under `operation`.
    pub fn get(&self, operation: &str) -> Option<Arc<Run>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).get(operation).and_then(Weak::upgrade)
    }
}

pub(super) struct Run {
    state: watch::Sender<Pending>,
}

#[derive(Default)]
pub(super) struct Pending {
    /// The gateway credential whose `chunk:command` started the command.
    owner: Option<String>,
    effects: BTreeMap<u32, Held>,
    finished: bool,
}

struct Held {
    value: CommandEffect,
    effect: chunk_backend::CommandEffect,
}

impl Run {
    /// Binds the command to the gateway `credential` starting it, unless another started it.
    pub fn start(&self, credential: &str) -> Result<(), Error> {
        let mut started = Ok(());
        self.state.send_if_modified(|pending| {
            if pending.owner.is_some() {
                started = pending.permits(credential);
                return false;
            }
            pending.owner = Some(credential.to_owned());
            true
        });
        started
    }

    /// Checks that `credential` started the command, or that it hasn't started.
    pub fn permits(&self, credential: &str) -> Result<(), Error> {
        self.state.borrow().permits(credential)
    }

    /// Holds `effect` for the gateway to render as `value`, or fails it while the command already has as many
    /// pending as it may, or finished.
    pub fn publish(&self, effect: chunk_backend::CommandEffect, value: CommandEffect) {
        let mut effect = Some(effect);
        self.state.send_if_modified(|pending| {
            pending.effects.retain(|_, held| !held.effect.is_cancelled());
            if pending.finished || pending.effects.len() >= PENDING {
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

    /// Resolves pending effect `sequence` with the gateway's acknowledgment, returning whether it was pending.
    pub fn acknowledge(&self, sequence: u32, failed: bool) -> bool {
        let mut held = None;
        self.state.send_if_modified(|pending| {
            held = pending.effects.remove(&sequence);
            held.is_some()
        });
        let Some(Held { effect, .. }) = held else { return false };
        if failed {
            effect.finish(None);
        } else {
            effect.accept();
        }
        true
    }

    /// Marks the command finished, failing the effects still pending.
    pub fn finish(&self) {
        let mut effects = BTreeMap::new();
        self.state.send_modify(|pending| {
            pending.finished = true;
            effects = std::mem::take(&mut pending.effects);
        });
        for held in effects.into_values() {
            held.effect.finish(None);
        }
    }

    pub fn follow(&self) -> watch::Receiver<Pending> {
        self.state.subscribe()
    }
}

impl Pending {
    /// Checks that `credential` started the command, or that it hasn't started.
    pub fn permits(&self, credential: &str) -> Result<(), Error> {
        match &self.owner {
            Some(owner) if !same_secret(owner, credential) => Err(errors::denied("another gateway ran this command")),
            _ => Ok(()),
        }
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// The pending effects as a snapshot.
    pub fn snapshot(&self) -> Update {
        let upserts = self.effects.iter().map(|(sequence, held)| Entry {
            key: sequence.to_string(),
            state: Some(State::Value(held.value.encode_to_vec())),
        });
        Update { snapshot: true, upserts: upserts.collect(), ..Update::default() }
    }
}
