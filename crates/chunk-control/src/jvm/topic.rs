use std::{collections::BTreeMap, sync::Arc};

use chunk_proto::{control::v1::ClaimRequest, sync::v1 as sync};
use prost::Message;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{Stream, Work};
use crate::{
    Control, Error, Generation, Result,
    state::{Capacity, Phase, State},
};

/// One stream of a JVM's topic. Every update is a snapshot, sent only when it differs from the previous one. Dropping
/// the stream ends it, detaching the link its reports attached.
pub struct Topic {
    control: Arc<Control>,
    host: String,
    stream: String,
    ended: CancellationToken,
    positions: watch::Receiver<Generation>,
    work: watch::Receiver<Work>,
    /// The entries last sent, with encoded values.
    sent: BTreeMap<String, Vec<u8>>,
}

impl Topic {
    /// Opens `host`'s topic as `stream`, which becomes the host's current stream and ends the earlier one, and returns
    /// its first snapshot.
    /// # Errors
    /// Rejects a host whose JVM has not registered over sync, and reports unreadable control state.
    pub fn open(control: &Arc<Control>, host: &str, stream: &str) -> Result<(Self, sync::Update)> {
        let positions = control.subscribe();
        let ended = CancellationToken::new();
        let work = {
            let mut jvms = control.jvms.lock()?;
            let jvm = jvms.get_mut(host).ok_or(Error::Invalid("the JVM has not registered over sync"))?;
            let current = Stream { id: stream.into(), link: None, ended: ended.clone() };
            if let Some(previous) = jvm.stream.replace(current) {
                previous.end(&control.links, host);
            }
            jvm.work.subscribe()
        };
        let mut topic = Self {
            control: control.clone(),
            host: host.into(),
            stream: stream.into(),
            ended,
            positions,
            work,
            sent: BTreeMap::new(),
        };
        let (position, entries) = topic.entries()?;
        let update = topic.snapshot(position, entries);
        Ok((topic, update))
    }

    /// Cancelled once the stream stops being current: a newer stream superseded it, or its JVM was stopped.
    #[must_use]
    pub fn ended(&self) -> CancellationToken {
        self.ended.clone()
    }

    /// Waits for a commit or for control to want other work of the JVM, or forever once control is gone.
    pub async fn changed(&mut self) {
        tokio::select! {
            Ok(()) = self.positions.changed() => {}
            Ok(()) = self.work.changed() => {}
            else => std::future::pending().await,
        }
    }

    /// A snapshot if control wants something else of the JVM than the previous update said, or else `None`.
    /// # Errors
    /// Reports a replaced process and unreadable control state.
    pub fn update(&mut self) -> Result<Option<sync::Update>> {
        let (position, entries) = self.entries()?;
        if entries == self.sent {
            return Ok(None);
        }
        let forgotten: Vec<String> = self
            .sent
            .keys()
            .filter(|key| !entries.contains_key(*key))
            .filter_map(|key| key.strip_prefix("session/"))
            .map(Into::into)
            .collect();
        self.control.links.forget(&self.host, &forgotten);
        Ok(Some(self.snapshot(position, entries)))
    }

    fn entries(&self) -> Result<(Generation, BTreeMap<String, Vec<u8>>)> {
        let state = self.control.state()?;
        if self.control.host.connection(&self.host).is_none() {
            return Err(Error::Invalid("unregistered or replaced process"));
        }
        let mut entries = BTreeMap::new();
        for (id, session) in crate::sync::desired(&state, &self.host)? {
            entries.insert(format!("session/{id}"), session.encode_to_vec());
        }
        for (operation, delivery) in deliveries(&state, &self.host)? {
            entries.insert(format!("delivery/{operation}"), delivery.encode_to_vec());
        }
        let work = self.work.borrow();
        for (operation, method) in work.methods.iter().filter(|(_, method)| method.result.is_none()) {
            entries.insert(format!("method/{operation}"), method.call.encode_to_vec());
        }
        let releasing = state.hosts.get(&self.host).is_some_and(|host| host.capacity == Capacity::Releasing);
        if releasing || work.stopping {
            entries.insert("stop".into(), sync::JvmStop {}.encode_to_vec());
        }
        Ok((state.position(), entries))
    }

    fn snapshot(&mut self, position: Generation, entries: BTreeMap<String, Vec<u8>>) -> sync::Update {
        let upserts = entries.iter().map(|(key, value)| sync::Entry {
            key: key.clone(),
            state: Some(sync::entry::State::Value(value.clone())),
        });
        let update = sync::Update {
            position: crate::gateway::position(position),
            snapshot: true,
            upserts: upserts.collect(),
            ..sync::Update::default()
        };
        self.sent = entries;
        update
    }
}

impl Drop for Topic {
    fn drop(&mut self) {
        let Ok(mut jvms) = self.control.jvms.lock() else {
            return;
        };
        if let Some(jvm) = jvms.get_mut(&self.host)
            && jvm.stream.as_ref().is_some_and(|current| current.id == self.stream)
            && let Some(current) = jvm.stream.take()
        {
            current.end(&self.control.links, &self.host);
        }
    }
}

/// The delivery of each open claim on `host`'s sessions, by operation ID. One whose claim is withdrawing asks the JVM
/// to close it; a delivery the JVM holds without an open claim is left out, so the JVM closes it.
fn deliveries(state: &State, host: &str) -> Result<BTreeMap<String, sync::JvmDelivery>> {
    let mut deliveries = BTreeMap::new();
    for (operation, claim) in &state.claims {
        if claim.phase == Phase::Released
            || state.sessions.get(&claim.session).is_none_or(|session| session.host != host)
        {
            continue;
        }
        let identity = ClaimRequest::decode(claim.request.as_slice())?.identity.unwrap_or_default();
        let properties = identity.properties.into_iter().map(|property| sync::PlayerProperty {
            name: property.name,
            value: property.value,
            signature: property.signature,
        });
        let player =
            sync::PlayerIdentity { uuid: identity.uuid, username: identity.username, properties: properties.collect() };
        let delivery = sync::JvmDelivery {
            session: claim.session.clone(),
            generation: crate::gateway::position(claim.generation),
            player: Some(player),
            withdraw: claim.phase == Phase::Withdrawing,
        };
        deliveries.insert(operation.clone(), delivery);
    }
    Ok(deliveries)
}
