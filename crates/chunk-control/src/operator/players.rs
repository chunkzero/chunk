//! The `players` topic: each player with a current claim, keyed by UUID, with `chunk.sync.v1.OperatorPlayer` values.
//! A commit only re-reads the players whose claims or moves it touched.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chunk_proto::{sync::v1 as sync, v1::ClaimRequest};
use prost::Message;
use tokio::sync::watch;

use super::{changed, demand, entry};
use crate::{
    Control, Generation, Result,
    gateway::{phase, position},
    state::{MoveIntent, Phase, State, feed::Change},
};

/// One stream of the `players` topic.
pub struct Players {
    control: Arc<Control>,
    positions: watch::Receiver<Generation>,
    /// The value last sent for each player.
    sent: BTreeMap<String, sync::OperatorPlayer>,
    /// The position of the last update.
    position: Generation,
}

impl Players {
    /// Opens a stream and returns its first update, a snapshot.
    /// # Errors
    /// Reports unreadable control state.
    pub fn open(control: &Arc<Control>) -> Result<(Self, sync::Update)> {
        let mut players = Self {
            control: control.clone(),
            positions: control.subscribe(),
            sent: BTreeMap::new(),
            position: Generation::default(),
        };
        let state = control.state()?;
        let first = players.snapshot(&state)?;
        Ok((players, first))
    }

    /// Resolves once control committed.
    pub async fn changed(&mut self) {
        changed(&mut self.positions).await;
    }

    /// The players that changed since the previous update, or `None` when control has not committed since. A commit
    /// that touched none of them only advances the position, and one outside retained history sends a new snapshot.
    /// # Errors
    /// Reports unreadable or undecodable control state.
    pub fn update(&mut self) -> Result<Option<sync::Update>> {
        match self.control.authority.feed().after(self.position) {
            Some((_, state)) if state.position() == self.position => Ok(None),
            Some((changes, state)) => self.changes(&state, &changes).map(Some),
            None => {
                let state = self.control.state()?;
                self.snapshot(&state).map(Some)
            }
        }
    }

    fn snapshot(&mut self, state: &State) -> Result<sync::Update> {
        let mut sent = BTreeMap::new();
        for uuid in state.players.keys() {
            if let Some(player) = player(state, uuid)? {
                sent.insert(uuid.clone(), player);
            }
        }
        self.sent = sent;
        self.position = state.position();
        let upserts = self.sent.iter().map(|(uuid, player)| entry(uuid.clone(), player)).collect();
        Ok(sync::Update { position: position(self.position), snapshot: true, upserts, ..sync::Update::default() })
    }

    /// The players whose claims `changes` touched, read from `state`, which they lead to.
    fn changes(&mut self, state: &State, changes: &[Change]) -> Result<sync::Update> {
        let touched: BTreeSet<_> = changes.iter().map(|change| change.player.as_str()).collect();
        self.position = state.position();
        let mut update = sync::Update { position: position(self.position), ..sync::Update::default() };
        for uuid in touched {
            match player(state, uuid)? {
                Some(player) if self.sent.get(uuid) != Some(&player) => {
                    update.upserts.push(entry(uuid.to_owned(), &player));
                    self.sent.insert(uuid.to_owned(), player);
                }
                Some(_) => {}
                None => {
                    if self.sent.remove(uuid).is_some() {
                        update.removed.push(uuid.to_owned());
                    }
                }
            }
        }
        Ok(update)
    }
}

/// `uuid`'s entry, unless they hold no current claim. Only their current claim's moves are decoded.
fn player(state: &State, uuid: &str) -> Result<Option<sync::OperatorPlayer>> {
    let Some(owner) = state.players.get(uuid) else { return Ok(None) };
    let Some((operation, claim)) =
        owner.current.as_ref().and_then(|operation| Some((operation, state.claims.get(operation)?)))
    else {
        return Ok(None);
    };
    let request = ClaimRequest::decode(claim.request.as_slice())?;
    let source = claim.identity(operation);
    let mut latest: Option<(&MoveIntent, ClaimRequest)> = None;
    for intent in state.move_sources.get(operation).into_iter().flatten().filter_map(|id| state.moves.get(id)) {
        let destination = ClaimRequest::decode(intent.request.as_slice())?;
        if destination.source.as_ref() == Some(&source)
            && latest.as_ref().is_none_or(|(latest, _)| intent.sequence >= latest.sequence)
        {
            latest = Some((intent, destination));
        }
    }
    let last_move_failure = latest.as_ref().and_then(|(intent, destination)| {
        intent.failure.as_ref().map(|failure| sync::MoveFailure {
            destination: destination.demand.clone().map(demand),
            reason: failure.reason.clone(),
            failed_at_ms: failure.at_ms,
        })
    });
    let queued = latest.as_ref().is_some_and(|(intent, destination)| {
        !intent.canceled
            && intent.failure.is_none()
            && state
                .claims
                .get(&destination.operation_id)
                .is_none_or(|claim| !matches!(claim.phase, Phase::Withdrawing | Phase::Released))
    });
    let host = state.sessions.get(&claim.session).map(|session| session.host.clone()).unwrap_or_default();
    Ok(Some(sync::OperatorPlayer {
        username: request.identity.map(|identity| identity.username).unwrap_or_default(),
        demand: request.demand.map(demand),
        app: state.hosts.get(&host).map(|host| host.app.clone()).unwrap_or_default(),
        host,
        phase: phase(claim.phase).into(),
        moving: owner.pending.is_some() || queued,
        since_ms: claim.created_at_ms,
        last_move_failure,
    }))
}
