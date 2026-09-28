//! Recovery after control restarts: JVMs that outlived it re-attach, and the log decides which of their deliveries
//! and sessions remain owned. Until every surviving JVM's deliveries are fenced and the sessions the log lost are
//! finished, or its host confirms it stopped, new claims are refused as busy, so a player a JVM still serves without a
//! claim in the log cannot gain a second owner. No timeout reopens admission: an unreachable JVM may still be serving
//! players.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::{Duration, Instant},
};

use chunk_proto::{
    sync::v1::{JvmDeliveryPhase, JvmDeliveryStatus, JvmReport, JvmSessionPhase},
    v1::ShutdownNodeRequest,
};
use prost::Message;

use crate::{
    Control, Error, JvmIdentity, Registration, Result, RuntimeConnection,
    drain::retire_host,
    placement::runs_host,
    state::{Capacity, Claim, Generation, HostState, Phase, SessionState, State},
};

/// How often admission warns that it still waits for surviving JVMs. JVMs repeat registration every second.
const RECOVERY_WARNING: Duration = Duration::from_secs(30);

/// Hosts whose surviving deliveries are not yet fenced.
pub(crate) struct Recovery {
    pending: Mutex<Pending>,
    resolving: Mutex<()>,
}

struct Pending {
    /// Each pending host, with the re-attachment count when it last became pending. A resolution that started before a
    /// later re-attachment does not complete it.
    hosts: BTreeMap<String, u64>,
    attachments: u64,
    /// When admission next warns that hosts are still pending.
    deadline: Instant,
}

impl Recovery {
    pub fn new(hosts: BTreeSet<String>) -> Self {
        let hosts = hosts.into_iter().map(|host| (host, 0)).collect();
        let pending = Pending { hosts, attachments: 0, deadline: Instant::now() + RECOVERY_WARNING };
        Self { pending: Mutex::new(pending), resolving: Mutex::new(()) }
    }

    #[cfg(test)]
    pub fn pass_deadline(&self) {
        self.pending.lock().unwrap().deadline = Instant::now();
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Pending>> {
        self.pending.lock().map_err(|_| Error::Unresolved("recovery poisoned"))
    }

    pub fn open(&self) -> Result<bool> {
        Ok(self.lock()?.hosts.is_empty())
    }

    /// Hosts still pending, including a re-attached orphan whose host row its first report has yet to recreate.
    pub fn pending(&self) -> Result<Vec<String>> {
        Ok(self.lock()?.hosts.keys().cloned().collect())
    }

    /// Runs `adopt`, then makes `host` pending again, under the lock resolution completes hosts under. A resolution
    /// that observed the adopted process therefore sees the new stamp and leaves the host pending.
    fn reattach(&self, host: &str, adopt: impl FnOnce() -> Result<()>) -> Result<()> {
        let mut pending = self.lock()?;
        adopt()?;
        pending.attachments += 1;
        let attachment = pending.attachments;
        pending.hosts.insert(host.into(), attachment);
        Ok(())
    }
}

impl Control {
    /// Admits new claims once recovery has resolved, first trying to resolve it.
    /// # Errors
    /// Reports `Busy` while a surviving JVM's deliveries remain unfenced.
    pub(crate) fn admit(&self) -> Result<()> {
        self.resolve_recovery()?;
        if self.recovery.open()? { Ok(()) } else { Err(Error::Busy) }
    }

    /// Fences each pending host's surviving deliveries. Returns at once while another caller is resolving.
    pub(crate) fn resolve_recovery(&self) -> Result<()> {
        if self.recovery.open()? {
            return Ok(());
        }
        let Ok(_resolving) = self.recovery.resolving.try_lock() else {
            return Ok(());
        };
        let hosts = self.recovery.lock()?.hosts.clone();
        for (id, attachment) in hosts {
            let resolved = match self.recover_host(&id) {
                Ok(resolved) => resolved,
                Err(error) => {
                    tracing::debug!(%error, host = id, "surviving deliveries remain unfenced");
                    false
                }
            };
            let mut pending = self.recovery.lock()?;
            if resolved && pending.hosts.get(&id) == Some(&attachment) {
                pending.hosts.remove(&id);
            }
        }
        let mut pending = self.recovery.lock()?;
        if !pending.hosts.is_empty() && Instant::now() >= pending.deadline {
            pending.deadline = Instant::now() + RECOVERY_WARNING;
            tracing::warn!(
                hosts = ?pending.hosts.keys().collect::<Vec<_>>(),
                "new claims wait until surviving JVMs re-attach or their hosts confirm they stopped"
            );
        }
        Ok(())
    }

    fn recover_host(&self, id: &str) -> Result<bool> {
        let state = self.state()?;
        // Only a host missing now is an orphan; one removed later, after its capacity was released, is not.
        let orphan = !state.hosts.contains_key(id);
        // The log has no capacity record for a host a restore lost, so only the host can confirm its JVM exited.
        if state.hosts.get(id).map_or_else(|| self.host.stopped(id), |host| host.capacity == Capacity::Released) {
            return Ok(true);
        }
        let Some(runtime) = self.host.connection(id) else {
            // Without an unowned launch, no JVM from before the restart can run on this host.
            return Ok(!self.host.unresolved(id));
        };
        if let Some(host) = state.hosts.get(id)
            && !runs_host(&state, &runtime, host)
        {
            return Err(Error::Invalid("recovered runtime mismatch"));
        }
        // The JVM's complete report, from a stream it opened after re-attaching.
        let Some(inventory) = self.links.report(id, &runtime.identity) else {
            return Ok(false);
        };
        self.retire_unknown_operations(id, &runtime.identity)?;
        self.retire_unknown_sessions(id, &runtime.identity)?;
        if !self.fenced(id, &inventory)? {
            return Ok(false);
        }
        if orphan {
            self.retire_orphan(id, &runtime.identity)?;
        }
        // Control's desired state asks the JVM to end the sessions the log lost; its reports finish them.
        let state = self.state()?;
        Ok(!state.sessions.values().any(|session| session.host == id && session.recovered() && !session.finished))
    }

    /// Records each session a logged host's JVM runs without a log row, such as one whose creation a restore lost, as
    /// a retired session to finish. Until the JVM confirms it ended, its row counts toward the host's capacity. A lost
    /// orphan host's sessions end with its JVM instead.
    fn retire_unknown_sessions(&self, id: &str, identity: &JvmIdentity) -> Result<()> {
        self.update(|state| {
            // A released host's JVM has exited, and its sessions with it.
            let live = state.hosts.get(id).is_some_and(|host| host.capacity != Capacity::Released);
            let Some(inventory) = self.links.report(id, identity).filter(|_| live) else {
                return Ok(());
            };
            for observed in &inventory.sessions {
                let ended =
                    observed.phase() == JvmSessionPhase::Ended && observed.prepared == 0 && observed.attached == 0;
                if ended || state.sessions.contains_key(&observed.id) {
                    continue;
                }
                let tombstone = SessionState {
                    empty_since_ms: None,
                    finish_requested: true,
                    finished: false,
                    host: id.into(),
                    session_type: observed.session_type.clone(),
                    demand_key: String::new(),
                    capacity: observed.capacity,
                    configuration: serde_json::json!({}),
                    retired: true,
                };
                state.sessions.insert(observed.id.clone(), tombstone);
            }
            Ok(())
        })
    }

    /// Records a fenced JVM whose host row a restore lost as ready, retiring capacity of the release it names, so the
    /// host lifecycle stops it: its drain releases it at once, and control shutdown stops it like any logged host. The
    /// release need not be known: after another restart, the JVM re-attaches to this row by its launch record alone.
    fn retire_orphan(&self, id: &str, identity: &JvmIdentity) -> Result<()> {
        self.update(|state| {
            if state.hosts.contains_key(id) {
                return Ok(());
            }
            state.hosts.insert(id.into(), orphan(identity));
            let operation = format!("orphan/{id}");
            let request =
                ShutdownNodeRequest { operation_id: operation.clone(), host_id: id.into(), timeout_seconds: 0 };
            retire_host(state, operation, request.encode_to_vec(), 0, true, |_| Ok(id.into()))
        })
    }

    /// Stops the JVM that outlived control on `id` for `reason` by releasing its host, recording a host row a restore
    /// lost as [`Self::retire_orphan`] does. The capacity executor retries the release until the host confirms the JVM
    /// exited, and recovery resolves the host only once its capacity is released.
    pub(crate) fn stop_recovered(&self, id: &str, identity: &JvmIdentity, reason: &str) -> Result<()> {
        self.update(|state| {
            state.hosts.entry(id.into()).or_insert_with(|| orphan(identity));
            crate::capacity::stop(state, id, Some(reason.into()));
            Ok(())
        })?;
        self.wake_capacity();
        Ok(())
    }

    /// Accepts a JVM registration. A JVM launched before control restarted re-attaches only if its host adopts it,
    /// which requires the credential recorded before launch, and it still runs its logged host's app. A JVM whose
    /// host row a restore lost re-attaches by its launch record alone; the log owns none of its deliveries, so
    /// recovery withdraws them all.
    pub(crate) fn register(&self, token: &str, registration: Registration) -> Result<()> {
        let error = match self.host.register(token, registration.clone()) {
            Ok(()) => {
                self.wake_capacity();
                return Ok(());
            }
            Err(error) => error,
        };
        let Some(secret) = token.strip_prefix("Bearer ") else {
            return Err(error);
        };
        let identity = registration.identity.clone();
        let state = self.state()?;
        if let Some(host) = state.hosts.get(&identity.host) {
            let connection = RuntimeConnection {
                token: secret.into(),
                identity: identity.clone(),
                player_endpoint: registration.player_endpoint.clone(),
            };
            if !runs_host(&state, &connection, host) {
                return Err(error);
            }
        }
        self.recovery.reattach(&identity.host, || self.host.adopt(secret, registration))?;
        self.wake_capacity();
        tracing::info!(host = identity.host, "re-attached a JVM that outlived control");
        Ok(())
    }

    /// Retires the operations of `id`'s surviving deliveries the log does not own, as [`retire_unowned`] does.
    fn retire_unknown_operations(&self, id: &str, identity: &JvmIdentity) -> Result<()> {
        self.update(|state| match self.links.report(id, identity) {
            Some(inventory) => retire_unowned(state, id, &inventory),
            None => Ok(()),
        })
    }

    /// Whether `host`'s JVM reported closed every delivery in `inventory` that no open claim on `host` owns with the
    /// same generations. Its topic leaves those deliveries out, so the JVM closes them itself. `inventory` must be
    /// reported before state is read, so every delivery control prepared already has its claim.
    pub(crate) fn fenced(&self, host: &str, inventory: &JvmReport) -> Result<bool> {
        let state = self.state()?;
        Ok(inventory
            .deliveries
            .iter()
            .filter(|status| status.phase() != JvmDeliveryPhase::Closed)
            .all(|status| crate::jvm::owned(&state, host, status)))
    }
}

/// A ready host row for the JVM `identity` names, whose row a restore lost.
fn orphan(identity: &JvmIdentity) -> HostState {
    HostState {
        capacity: Capacity::Ready,
        ..HostState::requested(&identity.deployment, &identity.app, &identity.profile)
    }
}

/// Records a released tombstone for each delivery surviving on `host` whose operation the log does not know, such as
/// deliveries prepared by commits a restore lost. A retry of that operation is then rejected instead of reserving the
/// player again under an operation ID the JVM already holds. A reservation on `host` whose assignment a restore lost
/// is released as such a tombstone too, whatever phase its delivery reached: control can't adopt a delivery it never
/// recorded the capability of, so it fences it before admission reopens.
pub(crate) fn retire_unowned(state: &mut State, host: &str, inventory: &JvmReport) -> Result<()> {
    for status in &inventory.deliveries {
        let operation = &status.operation_id;
        if crate::jvm::owned(state, host, status)
            && state.claims.get(operation).is_some_and(|claim| claim.assignment.is_none())
        {
            crate::delivery::release(state, operation)?;
            state.claims.get_mut(operation).ok_or(Error::Invalid("unknown claim"))?.request.clear();
        }
        retire_unknown(state, status);
    }
    Ok(())
}

/// Records a released claim standing for the operation of the delivery `status` reports unless the log knows it. Its
/// empty request matches no retry.
pub(crate) fn retire_unknown(state: &mut State, status: &JvmDeliveryStatus) {
    state.claims.entry(status.operation_id.clone()).or_insert_with(|| tombstone(status));
}

fn tombstone(status: &JvmDeliveryStatus) -> Claim {
    let now = crate::now_ms();
    Claim {
        request: Vec::new(),
        player: String::new(),
        proxy: String::new(),
        membership: Generation::default(),
        generation: crate::jvm::generation(status).unwrap_or_default(),
        session: String::new(),
        phase: Phase::Released,
        assignment: None,
        activated: false,
        created_at_ms: now,
        released_at_ms: Some(now),
        roster: None,
    }
}
