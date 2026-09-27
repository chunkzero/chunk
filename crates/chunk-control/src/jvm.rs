//! JVMs attached over the sync protocol. The `jvm/<host>` topic carries the sessions control wants the JVM to run,
//! keyed `session/<id>` with `chunk.sync.v1.JvmSession` values, and a `stop` entry once its host is releasing. The
//! JVM's registration and reports go through the same registration, attach and report path as the supervisor's.
//!
//! Each host's entry, under one lock, is the only authority for which of its topic streams is current: opening a stream
//! ends the previous one, and reports check their stream and apply their effects inside that lock.

use std::{
    collections::{BTreeMap, btree_map},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use chunk_proto::{
    sync::v1 as sync,
    v1::{
        DeploymentRef, ProcessHealth, ProcessIdentity, ProcessRegistration, ProcessReport, SessionInventory,
        SessionPhase, SessionRef,
    },
};
use prost::Message;
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{Control, Error, Generation, Result, state::Capacity, sync::Links};

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
    health: Option<(Instant, ProcessHealth)>,
    /// Whether control asked the JVM to stop.
    stopping: watch::Sender<bool>,
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

    /// Whether `host`'s JVM registered over sync, so it pushes its health in reports instead of serving the probe.
    pub fn pushes_health(&self, host: &str) -> bool {
        self.0.lock().is_ok_and(|jvms| jvms.contains_key(host))
    }

    /// The health `host`'s JVM last pushed, unless it has pushed none for 10 seconds.
    pub fn health(&self, host: &str) -> Option<ProcessHealth> {
        let jvms = self.0.lock().ok()?;
        let (sampled, health) = jvms.get(host)?.health.as_ref()?;
        (sampled.elapsed() <= HEALTH).then(|| health.clone())
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
        let changed = || Err(Error::Invalid("registration changed"));
        if self.jvms.lock()?.get(host).is_some_and(|jvm| jvm.registration != registration) {
            return changed();
        }
        // Registering may commit, so it runs outside the JVMs' lock.
        self.register(&format!("Bearer {credential}"), self.process(host, &registration))?;
        match self.jvms.lock()?.entry(host.into()) {
            btree_map::Entry::Occupied(jvm) if jvm.get().registration != registration => changed(),
            btree_map::Entry::Occupied(_) => Ok(()),
            btree_map::Entry::Vacant(entry) => {
                let stopping = watch::Sender::new(false);
                entry.insert(Jvm { registration, stream: None, health: None, stopping });
                Ok(())
            }
        }
    }

    /// Re-attaches the JVM that outlived control on `host`, whose credential is `credential`, only to stop it, as when
    /// a restore lost the deployment it runs. Its launch record authenticates it as a registration does, so its host
    /// kills it by its verified PID once the stop grace passes, and recovery counts the host resolved once it exits.
    /// # Errors
    /// Rejects a registration its launch record or the log does not match.
    pub fn stop_survivor(
        self: &Arc<Self>,
        host: &str,
        credential: &str,
        registration: &sync::JvmRegistration,
    ) -> Result<()> {
        self.register(&format!("Bearer {credential}"), self.process(host, registration))?;
        let (control, host) = (self.clone(), host.to_owned());
        tokio::spawn(async move {
            match control.release_host(&host).await {
                Ok(true) => tracing::info!(host, "stopped a JVM whose deployment is gone"),
                Ok(false) => tracing::warn!(host, "a JVM whose deployment is gone may still run"),
                Err(error) => tracing::warn!(%error, host, "cannot stop a JVM whose deployment is gone"),
            }
        });
        Ok(())
    }

    /// The supervisor registration `registration` stands for: `host`'s process, which serves no control endpoint.
    fn process(&self, host: &str, registration: &sync::JvmRegistration) -> ProcessRegistration {
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
        ProcessRegistration {
            identity: Some(identity),
            control_endpoint: String::new(),
            player_endpoint: registration.player_endpoint.clone(),
        }
    }

    /// Commits `report` from the JVM running `host`, sent on its topic stream `stream`, through the supervisor's
    /// attach and report path. The commit checks that `stream` is current and applies the report's sessions, link and
    /// health under the JVMs' lock, so a superseded stream changes nothing and each stream attaches once, with its
    /// first complete report. A report abandoned after that commit leaves its recovery to later reports and claims.
    /// # Errors
    /// Reports a superseded stream as stopped, and rejects a stream's reports before its first complete one.
    pub async fn report_jvm(&self, host: &str, credential: &str, stream: &str, report: sync::JvmReport) -> Result<()> {
        let runtime = self.host.connection(host).ok_or(Error::Invalid("unregistered or replaced process"))?;
        let sessions = report.sessions.into_iter().map(inventory).collect();
        let inventory = ProcessReport { identity: Some(runtime.identity.clone()), sessions, deliveries: Vec::new() };
        let health = report.health.map(|health| process_health(runtime.identity, health));
        let attached = self.update(|state| {
            let mut jvms = self.jvms.lock()?;
            let jvm = jvms.get_mut(host).ok_or(Error::Stopped)?;
            let current = jvm.stream.as_mut().filter(|current| current.id == stream).ok_or(Error::Stopped)?;
            let attached = match current.link {
                Some(link) => {
                    self.merge_in(state, host, link, &inventory)?;
                    None
                }
                None if report.complete => {
                    let (link, runtime) = self.attach_in(state, host, credential, &inventory)?;
                    current.link = Some(link);
                    Some(runtime)
                }
                None => return Err(Error::Invalid("a stream's first report must be complete")),
            };
            if let Some(health) = health {
                jvm.health = Some((Instant::now(), health));
            }
            Ok(attached)
        });
        self.links.applied();
        if let Some(runtime) = attached? {
            self.fence_deliveries(&runtime, &inventory).await?;
        }
        self.resolve_recovery().await
    }

    /// Stops `host`'s runtime through its host. A JVM registered over sync is first asked to stop on its topic, and
    /// keeps its credential until its stream closes or the grace a stop request has passes; then its stream ends.
    pub(crate) async fn release_host(&self, host: &str) -> Result<bool> {
        let stream = self.jvms.lock()?.get(host).map(|jvm| {
            jvm.stopping.send_replace(true);
            jvm.stream.as_ref().map(|stream| stream.ended.clone())
        });
        if let Some(Some(ended)) = stream {
            let _ = tokio::time::timeout(crate::process::STOP_GRACE, ended.cancelled()).await;
        }
        let released = self.host.release(host).await;
        if let Some(Some(stream)) = self.jvms.lock()?.get_mut(host).map(|jvm| jvm.stream.take()) {
            stream.end(&self.links, host);
        }
        released
    }
}

/// One stream of a JVM's topic. Every update is a snapshot, sent only when it differs from the previous one. Dropping
/// the stream ends it, detaching the link its reports attached.
pub struct Topic {
    control: Arc<Control>,
    host: String,
    stream: String,
    ended: CancellationToken,
    positions: watch::Receiver<Generation>,
    stopping: watch::Receiver<bool>,
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
        let stopping = {
            let mut jvms = control.jvms.lock()?;
            let jvm = jvms.get_mut(host).ok_or(Error::Invalid("the JVM has not registered over sync"))?;
            let current = Stream { id: stream.into(), link: None, ended: ended.clone() };
            if let Some(previous) = jvm.stream.replace(current) {
                previous.end(&control.links, host);
            }
            jvm.stopping.subscribe()
        };
        let mut topic = Self {
            control: control.clone(),
            host: host.into(),
            stream: stream.into(),
            ended,
            positions,
            stopping,
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

    /// Waits for a commit or for control to ask the JVM to stop, or forever once control is gone.
    pub async fn changed(&mut self) {
        tokio::select! {
            Ok(()) = self.positions.changed() => {}
            Ok(()) = self.stopping.changed() => {}
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
        let releasing = state.hosts.get(&self.host).is_some_and(|host| host.capacity == Capacity::Releasing);
        if releasing || *self.stopping.borrow() {
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
