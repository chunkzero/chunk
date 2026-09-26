//! Control's published state and the recent claim changes that led to it, so a subscriber can resume from the last
//! log position it saw.

use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, RwLock},
};

use chunk_proto::v1::ClaimRequest;
use chunk_store::DocumentKey;
use prost::Message;
use tokio::sync::watch;

use super::{
    Generation, State,
    store::{CLAIMS, MOVES},
};
use crate::{Error, Result};

/// Changes retained for resuming subscribers; older positions must reload current state.
const RETAINED: usize = 4096;

/// A claim whose state as its gateway watches it a commit may have changed. Read the state for its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    pub position: Generation,
    /// The proxy that holds the claim.
    pub gateway: String,
    /// The claim's operation ID.
    pub claim: String,
}

/// The claims that written `keys` may change, with their gateways: each written claim, and the source of each written
/// move and move destination, since those decide whether the source has a pending move.
pub(super) fn touched<'a>(
    previous: &State,
    next: &State,
    keys: impl Iterator<Item = &'a DocumentKey>,
) -> BTreeSet<(String, String)> {
    let claim = |id: &str| next.claims.get(id).or_else(|| previous.claims.get(id));
    let mut touched = BTreeSet::new();
    for key in keys {
        let request = match key.table.as_str() {
            CLAIMS => {
                let Some(written) = claim(&key.id) else { continue };
                touched.insert((written.proxy.clone(), key.id.clone()));
                &written.request
            }
            MOVES => match next.moves.get(&key.id).or_else(|| previous.moves.get(&key.id)) {
                Some(intent) => &intent.request,
                None => continue,
            },
            _ => continue,
        };
        let source = ClaimRequest::decode(request.as_slice()).ok().and_then(|request| request.source);
        if let Some(source) = source
            && let Some(held) = claim(&source.operation_id)
        {
            touched.insert((held.proxy.clone(), source.operation_id));
        }
    }
    touched
}

pub(crate) struct Feed {
    published: RwLock<Published>,
    position: watch::Sender<Generation>,
}

struct Published {
    /// The last committed state; its position is the latest change's, or later.
    state: Arc<State>,
    changes: VecDeque<Change>,
    /// Every change up to and including this position has been evicted.
    floor: Generation,
}

impl Feed {
    pub(super) fn new(state: State) -> Self {
        let position = state.position();
        let published = Published { state: Arc::new(state), changes: VecDeque::new(), floor: position };
        Self { published: RwLock::new(published), position: watch::Sender::new(position) }
    }

    /// Publishes a committed state with the claims its commit touched, then announces its position.
    pub(super) fn record(&self, state: State, touched: BTreeSet<(String, String)>) -> Result<()> {
        let position = state.position();
        {
            let mut published = self.write()?;
            published.state = Arc::new(state);
            let changes = touched.into_iter().map(|(gateway, claim)| Change { position, gateway, claim });
            published.changes.extend(changes);
            while published.changes.len() > RETAINED {
                if let Some(evicted) = published.changes.pop_front() {
                    published.floor = evicted.position;
                }
            }
        }
        self.position.send_replace(position);
        Ok(())
    }

    /// Publishes state reloaded from storage and forgets history, so earlier positions resynchronize.
    pub(super) fn reset(&self, state: State) -> Result<()> {
        let position = state.position();
        *self.write()? = Published { state: Arc::new(state), changes: VecDeque::new(), floor: position };
        self.position.send_replace(position);
        Ok(())
    }

    pub(super) fn state(&self) -> Result<Arc<State>> {
        Ok(self.published.read().map_err(|_| Error::Unresolved("control state poisoned"))?.state.clone())
    }

    /// The changes after `position` and the state they lead to, read together. `None` when that position is outside
    /// retained history: from another epoch, too old, or not yet committed.
    pub fn after(&self, position: Generation) -> Option<(Vec<Change>, Arc<State>)> {
        let published = self.published.read().ok()?;
        let current = published.state.position();
        if position.epoch != current.epoch || position < published.floor || position > current {
            return None;
        }
        let changes = published.changes.iter().filter(|change| change.position > position).cloned().collect();
        Some((changes, published.state.clone()))
    }

    pub fn subscribe(&self) -> watch::Receiver<Generation> {
        self.position.subscribe()
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Published>> {
        self.published.write().map_err(|_| Error::Unresolved("control state poisoned"))
    }
}
