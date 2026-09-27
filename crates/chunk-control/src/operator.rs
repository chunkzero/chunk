//! The operator's view of control over the sync protocol: the `nodes` and `players` topics, each diffed against what
//! its stream already sent, and the operator's drain. Values are `chunk.sync.v1` messages.

mod nodes;
mod players;

pub use nodes::Nodes;
pub use players::Players;

use chunk_proto::{sync::v1 as sync, v1::SessionDemand};
use prost::Message;
use tokio::sync::watch;

use crate::{
    Control, Error, Result,
    drain::{player_host, retire_host},
};

impl Control {
    /// Retires the host `arguments` names under the operator's `operation`, until its drain deadline. A retry returns
    /// the first drain's host and deadline.
    /// # Errors
    /// Rejects invalid arguments, an unknown host, a player without a current claim or whose host is already retired,
    /// and an operation first used for another drain with [`crate::DRAIN_CHANGED`].
    pub fn drain_operator(&self, operation: &str, arguments: &sync::DrainArguments) -> Result<sync::DrainResult> {
        use sync::drain_arguments::Target;
        let timeouts = match &arguments.target {
            Some(Target::Host(_)) => 0..=120,
            Some(Target::Player(_)) => 10..=120,
            None => return Err(Error::Invalid("invalid drain request")),
        };
        if operation.is_empty() || operation.len() > 128 || !timeouts.contains(&arguments.timeout_seconds) {
            return Err(Error::Invalid("invalid drain request"));
        }
        let key = format!("operator/{operation}");
        self.update(|state| {
            retire_host(state, key.clone(), arguments.encode_to_vec(), arguments.timeout_seconds, false, |state| {
                match &arguments.target {
                    Some(Target::Host(host)) if state.hosts.contains_key(host) => Ok(host.clone()),
                    Some(Target::Player(player)) => player_host(state, player),
                    _ => Err(Error::Invalid("unknown node")),
                }
            })?;
            let drain = &state.drains[&key];
            Ok(sync::DrainResult { host: drain.host.clone(), deadline_ms: drain.deadline_ms })
        })
    }
}

fn demand(demand: SessionDemand) -> sync::SessionDemand {
    sync::SessionDemand { key: demand.key, session_type: demand.session_type, machine_profile: demand.machine_profile }
}

fn entry(key: String, value: &impl Message) -> sync::Entry {
    sync::Entry { key, state: Some(sync::entry::State::Value(value.encode_to_vec())) }
}

/// Waits for `receiver` to change, or forever once its sender is gone.
async fn changed<T>(receiver: &mut watch::Receiver<T>) {
    if receiver.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}
