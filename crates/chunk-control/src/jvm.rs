//! JVMs attached over the sync protocol. The `jvm/<host>` topic carries the sessions control wants the JVM to run,
//! keyed `session/<id>` with `chunk.sync.v1.JvmSession` values, and a `stop` entry once its host is releasing. The
//! JVM's registration and reports go through the same registration, attach and report path as the supervisor's.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use chunk_proto::{
    sync::v1 as sync,
    v1::{
        DeploymentRef, ProcessHealth, ProcessIdentity, ProcessRegistration, ProcessReport, SessionInventory,
        SessionPhase, SessionRef,
    },
};
use prost::Message;

use crate::{Control, Error, Generation, Result, state::Capacity};

/// How long a JVM's last report stands for its health.
const HEALTH_MS: u64 = 10_000;

/// JVMs registered over the sync protocol, by host. Kept in memory: a JVM registers again after control restarts.
#[derive(Default)]
pub(crate) struct Jvms(Mutex<BTreeMap<String, Jvm>>);

struct Jvm {
    registration: sync::JvmRegistration,
    /// The host's current topic stream, with the link its first complete report attached.
    stream: Option<(String, Option<u64>)>,
    health: Option<ProcessHealth>,
    /// When the JVM last reported or registered, in Unix milliseconds.
    reported_ms: u64,
}

impl Jvms {
    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<String, Jvm>>> {
        self.0.lock().map_err(|_| Error::Unresolved("JVM registrations poisoned"))
    }

    /// Whether `host`'s JVM registered over sync, so it pushes its health in reports instead of serving the probe.
    pub fn pushes_health(&self, host: &str) -> bool {
        self.0.lock().is_ok_and(|jvms| jvms.contains_key(host))
    }

    /// The health `host`'s JVM last pushed, unless it has not reported for 10 seconds.
    pub fn health(&self, host: &str) -> Option<ProcessHealth> {
        let jvms = self.0.lock().ok()?;
        let jvm = jvms.get(host).filter(|jvm| crate::now_ms().saturating_sub(jvm.reported_ms) <= HEALTH_MS)?;
        jvm.health.clone()
    }

    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        if let Ok(mut jvms) = self.lock() {
            jvms.retain(|host, _| keep(host));
        }
    }
}

impl Control {
    /// Registers the JVM running `host`, whose credential is `credential`, like a supervisor registration without a
    /// control endpoint, so a JVM that outlived control re-attaches through it too. The first registration is frozen.
    /// # Errors
    /// Rejects a changed registration, or one the host or the log does not match.
    pub fn register_jvm(&self, host: &str, credential: &str, registration: sync::JvmRegistration) -> Result<()> {
        let mut jvms = self.jvms.lock()?;
        if jvms.get(host).is_some_and(|jvm| jvm.registration != registration) {
            return Err(Error::Invalid("registration changed"));
        }
        let identity = ProcessIdentity {
            deployment: Some(DeploymentRef {
                environment: self.config.environment.clone(),
                deployment: registration.deployment.clone(),
            }),
            runtime_id: host.into(),
            process_id: registration.process_id.clone(),
            generation: registration.generation,
            machine_profile: registration.profile.clone(),
            artifact_digest: registration.artifact_digest.clone(),
            app_id: registration.app.clone(),
        };
        let process = ProcessRegistration {
            identity: Some(identity),
            control_endpoint: String::new(),
            player_endpoint: registration.player_endpoint.clone(),
        };
        self.register(&format!("Bearer {credential}"), process)?;
        let reported_ms = crate::now_ms();
        jvms.entry(host.into()).or_insert(Jvm { registration, stream: None, health: None, reported_ms });
        Ok(())
    }

    /// Commits `report` from the JVM running `host`, sent on its topic stream `stream`, through the supervisor's
    /// attach and report path. The stream's first complete report attaches the JVM.
    /// # Errors
    /// Reports a superseded stream as stopped, and rejects a stream's reports before its first complete one.
    pub async fn report_jvm(&self, host: &str, credential: &str, stream: &str, report: sync::JvmReport) -> Result<()> {
        let link = {
            let jvms = self.jvms.lock()?;
            match jvms.get(host).and_then(|jvm| jvm.stream.as_ref()) {
                Some((current, link)) if current == stream => *link,
                _ => return Err(Error::Stopped),
            }
        };
        let runtime = self.host.connection(host).ok_or(Error::Invalid("unregistered or replaced process"))?;
        let identity = runtime.identity;
        let sessions = report.sessions.into_iter().map(inventory).collect();
        let inventory = ProcessReport { identity: Some(identity.clone()), sessions, deliveries: Vec::new() };
        match link {
            Some(link) => self.report(host, link, &inventory).await?,
            None if report.complete => {
                let link = self.attach(host, credential, inventory).await?;
                let mut jvms = self.jvms.lock()?;
                let current = jvms.get_mut(host).and_then(|jvm| jvm.stream.as_mut());
                let Some((_, attached)) = current.filter(|(current, _)| current == stream) else {
                    self.links.detach(host, link);
                    return Err(Error::Stopped);
                };
                *attached = Some(link);
            }
            None => return Err(Error::Invalid("a stream's first report must be complete")),
        }
        if let Some(jvm) = self.jvms.lock()?.get_mut(host) {
            jvm.reported_ms = crate::now_ms();
            if let Some(health) = report.health {
                jvm.health = Some(process_health(identity, health));
            }
        }
        Ok(())
    }
}

/// One stream of a JVM's topic. Every update is a snapshot, sent only when it differs from the previous one. Dropping
/// the stream detaches the link its reports attached.
pub struct Topic {
    control: Arc<Control>,
    host: String,
    stream: String,
    /// The entries last sent, with encoded values.
    sent: BTreeMap<String, Vec<u8>>,
}

impl Topic {
    /// Opens `host`'s topic as `stream`, which supersedes the host's earlier stream, and returns its first snapshot.
    /// # Errors
    /// Rejects a host whose JVM has not registered over sync, and reports unreadable control state.
    pub fn open(control: &Arc<Control>, host: &str, stream: &str) -> Result<(Self, sync::Update)> {
        {
            let mut jvms = control.jvms.lock()?;
            let jvm = jvms.get_mut(host).ok_or(Error::Invalid("the JVM has not registered over sync"))?;
            if let Some((_, Some(link))) = jvm.stream.replace((stream.into(), None)) {
                control.links.detach(host, link);
            }
        }
        let mut topic =
            Self { control: control.clone(), host: host.into(), stream: stream.into(), sent: BTreeMap::new() };
        let (position, entries) = topic.entries()?;
        let update = topic.snapshot(position, entries);
        Ok((topic, update))
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
        let runtime = self.control.host.connection(&self.host);
        let runtime = runtime.ok_or(Error::Invalid("unregistered or replaced process"))?;
        let mut entries = BTreeMap::new();
        for (id, (finish, command)) in crate::sync::desired(&state, &self.host, &runtime.identity)? {
            let session = sync::JvmSession {
                session_type: command.session_type,
                capacity: command.capacity,
                configuration_json: command.configuration_json,
                finish,
            };
            entries.insert(format!("session/{id}"), session.encode_to_vec());
        }
        if state.hosts.get(&self.host).is_some_and(|host| host.capacity == Capacity::Releasing) {
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
        let Some(jvm) = jvms.get_mut(&self.host) else {
            return;
        };
        if jvm.stream.as_ref().is_some_and(|(stream, _)| *stream == self.stream)
            && let Some((_, Some(link))) = jvm.stream.take()
        {
            self.control.links.detach(&self.host, link);
        }
    }
}

fn inventory(status: sync::JvmSessionStatus) -> SessionInventory {
    let phase = match status.phase() {
        sync::JvmSessionPhase::Unspecified => SessionPhase::Unspecified,
        sync::JvmSessionPhase::Starting => SessionPhase::Starting,
        sync::JvmSessionPhase::Ready => SessionPhase::Ready,
        sync::JvmSessionPhase::Ending => SessionPhase::Ending,
        sync::JvmSessionPhase::Ended => SessionPhase::Ended,
        sync::JvmSessionPhase::Failed => SessionPhase::Failed,
    };
    SessionInventory {
        session: Some(SessionRef { id: status.id }),
        generation: 1,
        session_type: status.session_type,
        phase: phase.into(),
        capacity: status.capacity,
        prepared: status.prepared,
        attached: status.attached,
    }
}

fn process_health(identity: ProcessIdentity, health: sync::JvmHealth) -> ProcessHealth {
    ProcessHealth {
        identity: Some(identity),
        ready: health.ready,
        draining: health.draining,
        tick_count: health.tick_count,
        last_tick_age_millis: health.last_tick_age_millis,
        heap_used_bytes: health.heap_used_bytes,
        heap_max_bytes: health.heap_max_bytes,
        gc_count: health.gc_count,
        gc_time_millis: health.gc_time_millis,
        process_cpu_load: health.process_cpu_load,
        sessions: health.sessions,
        players: health.players,
    }
}
