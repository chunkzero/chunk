//! JVMs attached over the sync protocol. The `jvm/<host>` topic carries the sessions control wants the JVM to run,
//! keyed `session/<id>` with `chunk.sync.v1.JvmSession` values; each open claim's delivery, keyed `delivery/<op>`; each
//! session method the JVM has yet to answer, keyed `method/<op>`; and a `stop` entry once its host is releasing.
//!
//! Each host's entry, under one lock, is the only authority for which of its topic streams is current: opening a stream
//! ends the previous one, and reports check their stream and apply their effects inside that lock. What the JVM
//! reported outlives its streams, in its link.

mod effects;
mod topic;

pub use topic::Topic;

use std::{
    collections::{BTreeMap, BTreeSet, btree_map},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use chunk_proto::sync::v1 as sync;
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{
    Control, Error, Generation, JvmIdentity, Registration, Result,
    state::{Phase, State},
    sync::Links,
};

/// How long a JVM's last health sample stands for its health.
const HEALTH: Duration = Duration::from_secs(10);

/// JVMs registered over the sync protocol, by host. Kept in memory: a JVM registers again after control restarts.
/// Reports take this lock inside a commit, so nothing may commit while holding it.
#[derive(Default)]
pub(crate) struct Jvms(Mutex<BTreeMap<String, Jvm>>);

struct Jvm {
    registration: sync::JvmRegistration,
    /// The host's current topic stream, the only one whose reports count.
    stream: Option<Stream>,
    /// The health the JVM last reported, and when.
    health: Option<(Instant, sync::JvmHealth)>,
    /// What control wants of the JVM beyond its log.
    work: watch::Sender<Work>,
    /// What the JVM last reported of each open claim's delivery at the claim's generation, by operation ID.
    deliveries: BTreeMap<String, Reported>,
}

/// A delivery's phase at `generation`, as its JVM reported it, with the capability it minted while prepared.
struct Reported {
    generation: Generation,
    phase: sync::JvmDeliveryPhase,
    capability: Vec<u8>,
}

/// The part of a JVM's topic that control keeps in memory.
#[derive(Default)]
pub(crate) struct Work {
    /// Whether control asked the JVM to stop.
    stopping: bool,
    /// Session methods by operation ID, within the JVM's method budget. The topic carries those still awaiting their
    /// result.
    methods: BTreeMap<String, Method>,
    /// The sequences of the methods whose results' retention ended, so a retry of one is unknown instead of asking
    /// the JVM again.
    retired: BTreeSet<u64>,
    /// Every sequence at or below this was retired too, once `retired` overflowed.
    retired_below: u64,
}

struct Method {
    sequence: u64,
    /// The call, without its arguments once answered.
    call: sync::JvmMethodCall,
    /// The result the JVM sent, and when.
    result: Option<(Instant, sync::JvmMethodResult)>,
}

struct Stream {
    id: String,
    /// The link the stream's first complete report attached.
    link: Option<u64>,
    /// Cancelled once the stream stops being the host's current one.
    ended: CancellationToken,
}

impl Stream {
    fn end(self, links: &Links, host: &str) {
        self.ended.cancel();
        if let Some(link) = self.link {
            links.detach(host, link);
        }
    }
}

impl Jvms {
    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<String, Jvm>>> {
        self.0.lock().map_err(|_| Error::Unresolved("JVM registrations poisoned"))
    }

    /// The health `host`'s JVM last pushed, unless it has pushed none for 10 seconds.
    pub fn health(&self, host: &str) -> Option<sync::JvmHealth> {
        let jvms = self.0.lock().ok()?;
        let (sampled, health) = jvms.get(host)?.health.as_ref()?;
        (sampled.elapsed() <= HEALTH).then_some(*health)
    }

    /// The Minecraft protocol `host`'s JVM registered with.
    pub fn protocol(&self, host: &str) -> Option<i32> {
        Some(self.0.lock().ok()?.get(host)?.registration.protocol)
    }

    /// The phase `host`'s JVM last reported for `operation`'s delivery at `generation`, with the capability it minted
    /// while prepared.
    pub fn delivery(
        &self,
        host: &str,
        operation: &str,
        generation: Generation,
    ) -> Option<(sync::JvmDeliveryPhase, Vec<u8>)> {
        let jvms = self.0.lock().ok()?;
        let reported = jvms.get(host)?.deliveries.get(operation)?;
        (reported.generation == generation).then(|| (reported.phase, reported.capability.clone()))
    }

    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        if let Ok(mut jvms) = self.lock() {
            jvms.retain(|host, _| keep(host));
        }
    }
}

impl Control {
    /// Registers the JVM running `host`, whose credential is `credential`. A JVM that outlived control re-attaches
    /// through it too. The first registration is frozen.
    /// # Errors
    /// Rejects a changed registration, or one the host or the log does not match.
    pub fn register_jvm(&self, host: &str, credential: &str, registration: sync::JvmRegistration) -> Result<()> {
        let changed = || Err(Error::Invalid("registration changed"));
        if self.jvms.lock()?.get(host).is_some_and(|jvm| jvm.registration != registration) {
            return changed();
        }
        // Registering may commit, so it runs outside the JVMs' lock.
        self.register(&format!("Bearer {credential}"), process(host, &registration))?;
        match self.jvms.lock()?.entry(host.into()) {
            btree_map::Entry::Occupied(jvm) if jvm.get().registration != registration => changed(),
            btree_map::Entry::Occupied(_) => Ok(()),
            btree_map::Entry::Vacant(entry) => {
                entry.insert(Jvm {
                    registration,
                    stream: None,
                    health: None,
                    work: watch::Sender::default(),
                    deliveries: BTreeMap::new(),
                });
                Ok(())
            }
        }
    }

    /// Re-attaches the JVM that outlived control on `host`, whose credential is `credential`, only to stop it, as when
    /// a restore lost the deployment it runs. Its launch record authenticates it as a registration does, and its host
    /// row, recreated if a restore lost it, is released, so its host kills it by its verified PID until it exits, and
    /// recovery counts the host resolved only then.
    /// # Errors
    /// Rejects a registration its launch record or the log does not match.
    pub fn stop_survivor(&self, host: &str, credential: &str, registration: &sync::JvmRegistration) -> Result<()> {
        let registration = process(host, registration);
        let identity = registration.identity.clone();
        self.register(&format!("Bearer {credential}"), registration)?;
        self.stop_recovered(host, &identity, "its deployment is gone")
    }

    /// Commits `report` from the JVM running `host`, sent on its topic stream `stream`. The commit checks that
    /// `stream` is current and applies the report's sessions, deliveries, link and health under the JVMs' lock, so a
    /// superseded stream changes nothing and each stream attaches once, with its first complete report. A report
    /// abandoned after that commit leaves its recovery to later reports and claims.
    /// # Errors
    /// Reports a superseded stream as stopped, and rejects a stream's reports before its first complete one and
    /// malformed deliveries.
    pub fn report_jvm(&self, host: &str, credential: &str, stream: &str, report: &sync::JvmReport) -> Result<()> {
        let prepared = |status: &sync::JvmDeliveryStatus| status.phase() == sync::JvmDeliveryPhase::Prepared;
        if report.deliveries.iter().any(|status| {
            status.operation_id.is_empty()
                || generation(status).is_none()
                || status.phase() == sync::JvmDeliveryPhase::Unspecified
                || (prepared(status) && status.capability.len() != 32)
        }) {
            return Err(Error::Invalid("invalid delivery status"));
        }
        let identity = self.host.connection(host).ok_or(Error::Invalid("unregistered or replaced process"))?.identity;
        let applied = self.update(|state| {
            let mut jvms = self.jvms.lock()?;
            let jvm = jvms.get_mut(host).ok_or(Error::Stopped)?;
            let current = jvm.stream.as_mut().filter(|current| current.id == stream).ok_or(Error::Stopped)?;
            match current.link {
                Some(link) => self.merge_in(state, host, link, &identity, report)?,
                None if report.complete => {
                    current.link = Some(self.attach_in(state, host, credential, &identity, report)?);
                }
                None => return Err(Error::Invalid("a stream's first report must be complete")),
            }
            if report.complete {
                jvm.deliveries.clear();
            }
            // Only a delivery at the generation of an open claim on this host counts, so a stale one or another host's
            // neither supplies nor removes the current one's capability.
            for status in report.deliveries.iter().filter(|status| owned(state, host, status)) {
                if let Some(generation) = generation(status) {
                    let reported =
                        Reported { generation, phase: status.phase(), capability: status.capability.clone() };
                    jvm.deliveries.insert(status.operation_id.clone(), reported);
                }
            }
            jvm.deliveries.retain(|operation, reported| {
                state
                    .claims
                    .get(operation)
                    .is_some_and(|claim| claim.phase != Phase::Released && claim.generation == reported.generation)
            });
            jvm.work.send_if_modified(|work| {
                work.prune();
                false
            });
            // A closed delivery no open claim owns needs nothing more, once a recovering host's operation the log
            // lost has its tombstone.
            let recovering = self.recovery.pending()?.iter().any(|pending| pending == host);
            let mut done = Vec::new();
            for status in &report.deliveries {
                if status.phase() == sync::JvmDeliveryPhase::Closed && !owned(state, host, status) {
                    if recovering {
                        crate::recovery::retire_unknown(state, status);
                    }
                    done.push(status.operation_id.clone());
                }
            }
            self.links.forget_deliveries(host, &done);
            if let Some(health) = report.health {
                jvm.health = Some((Instant::now(), health));
            }
            Ok(())
        });
        self.links.applied();
        applied?;
        self.resolve_recovery()
    }

    /// Stops `host`'s runtime through its host. A registered JVM is first asked to stop on its topic, and keeps its
    /// credential until its stream closes or the grace a stop request has passes; then its stream ends.
    pub(crate) async fn release_host(&self, host: &str) -> Result<bool> {
        let stream = self.jvms.lock()?.get(host).map(|jvm| {
            jvm.work.send_if_modified(|work| !std::mem::replace(&mut work.stopping, true));
            jvm.stream.as_ref().map(|stream| stream.ended.clone())
        });
        if let Some(Some(ended)) = stream {
            let _ = tokio::time::timeout(crate::process::STOP_GRACE, ended.cancelled()).await;
        }
        let released = self.host.release(host).await;
        if let Some(jvm) = self.jvms.lock()?.get_mut(host) {
            if let Some(stream) = jvm.stream.take() {
                stream.end(&self.links, host);
            }
            // Only a JVM its host confirmed stopped runs no more methods; one that may still run keeps their history.
            if matches!(released, Ok(true)) {
                jvm.work.send_if_modified(Work::forget_methods);
            }
        }
        released
    }
}

/// Whether an open claim on one of `host`'s sessions owns the delivery `status` reports, at the same generation.
pub(crate) fn owned(state: &State, host: &str, status: &sync::JvmDeliveryStatus) -> bool {
    state.claims.get(&status.operation_id).is_some_and(|claim| {
        claim.phase != Phase::Released
            && Some(claim.generation) == generation(status)
            && state.sessions.get(&claim.session).is_some_and(|session| session.host == host)
    })
}

/// The generation `status` reports, unless it is out of range.
pub(crate) fn generation(status: &sync::JvmDeliveryStatus) -> Option<Generation> {
    let position = status.generation.as_ref()?;
    Generation::new(position.epoch, position.revision).ok()
}

/// The registration `registration` stands for: `host`'s JVM.
fn process(host: &str, registration: &sync::JvmRegistration) -> Registration {
    let identity = JvmIdentity {
        host: host.into(),
        process_id: registration.process_id.clone(),
        generation: registration.generation,
        deployment: registration.deployment.clone(),
        app: registration.app.clone(),
        profile: registration.profile.clone(),
        artifact_digest: registration.artifact_digest.clone(),
    };
    Registration { identity, player_endpoint: registration.player_endpoint.clone() }
}
