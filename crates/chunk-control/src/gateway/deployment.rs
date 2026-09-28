//! The `deployment` sync topic: the current release's deployment, which gateways route new logins with. Its one entry
//! is keyed `current` with a `chunk.sync.v1.GatewayDeployment` value, and every update is a snapshot.

use chunk_proto::sync::v1 as sync;
use prost::Message;

use super::position;
use crate::{Control, Result, state::State};

/// One stream of the `deployment` topic.
pub struct Deployment {
    /// The deployment last sent, if a release was current then.
    sent: Option<String>,
}

impl Deployment {
    /// Opens a stream and returns its first update, a snapshot.
    /// # Errors
    /// Reports unreadable control state.
    pub fn open(control: &Control) -> Result<(Self, sync::Update)> {
        let state = control.state()?;
        let mut topic = Self { sent: None };
        let first = topic.snapshot(&state);
        Ok((topic, first))
    }

    /// A snapshot when the current release changed since the previous update, and otherwise `None`.
    /// # Errors
    /// Reports unreadable control state.
    pub fn next(&mut self, control: &Control) -> Result<Option<sync::Update>> {
        let state = control.state()?;
        Ok((state.current != self.sent).then(|| self.snapshot(&state)))
    }

    fn snapshot(&mut self, state: &State) -> sync::Update {
        self.sent.clone_from(&state.current);
        let upserts = state.current.iter().map(|deployment| sync::Entry {
            key: "current".into(),
            state: Some(sync::entry::State::Value(
                sync::GatewayDeployment { deployment: deployment.clone() }.encode_to_vec().into(),
            )),
        });
        sync::Update {
            position: position(state.position()),
            snapshot: true,
            upserts: upserts.collect(),
            ..sync::Update::default()
        }
    }
}
