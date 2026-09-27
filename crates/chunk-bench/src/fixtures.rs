//! Synthetic JVMs: one per host control launches, following the host's `jvm/<host>` topic inside the target process
//! as a JVM does over sync. Each runs every session at once, prepares every delivery, and arrives it once its claim
//! activates; there is no JVM startup, player socket or world simulation.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};

use anyhow::Result;
use chunk_control::{Control, Host, Progress, Release, RuntimeConnection, jvm::Topic};
use chunk_proto::{
    sync::v1::{
        self as sync, JvmDelivery, JvmDeliveryPhase, JvmDeliveryStatus, JvmHealth, JvmRegistration, JvmReport,
        JvmSession, JvmSessionPhase, JvmSessionStatus,
    },
    v1::{ClaimPhase, DeploymentRef, ProcessIdentity, ProcessRegistration},
};
use prost::Message;

const STREAM: &str = "synthetic";

/// How often a synthetic JVM pushes its health.
const HEALTH: Duration = Duration::from_secs(2);

pub fn identity(id: &str) -> ProcessIdentity {
    ProcessIdentity {
        deployment: Some(DeploymentRef { environment: "bench".into(), deployment: "bench".into() }),
        runtime_id: id.into(),
        process_id: format!("jvm-{id}"),
        generation: 1,
        machine_profile: "bench".into(),
        artifact_digest: "bench".into(),
        app_id: "bench".into(),
    }
}

fn credential(id: &str) -> String {
    format!("bench-{id}")
}

fn registration(id: &str) -> JvmRegistration {
    let identity = identity(id);
    JvmRegistration {
        process_id: identity.process_id,
        generation: identity.generation,
        app: identity.app_id,
        profile: identity.machine_profile,
        artifact_digest: identity.artifact_digest,
        deployment: "bench".into(),
        player_endpoint: "127.0.0.1:1".into(),
        protocol: 776,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("synthetic host lock")
}

#[derive(Default)]
pub struct SyntheticHost {
    control: OnceLock<Weak<Control>>,
    registered: Mutex<BTreeSet<String>>,
    followed: Mutex<BTreeSet<String>>,
    stopped: Mutex<BTreeSet<String>>,
}

impl SyntheticHost {
    /// Runs a synthetic JVM for each host `control` launches from now on.
    pub fn attach(&self, control: &Arc<Control>) {
        let _ = self.control.set(Arc::downgrade(control));
    }
}

#[tonic::async_trait]
impl Host for SyntheticHost {
    async fn ensure(&self, id: &str, _: &Release, _: &str, _: &str) -> chunk_control::Result<Progress> {
        if self.stopped(id) {
            return Ok(Progress::Failed("synthetic runtime stopped".into()));
        }
        if let Some(control) = self.control.get().and_then(Weak::upgrade)
            && lock(&self.followed).insert(id.into())
        {
            let id = id.to_owned();
            tokio::spawn(async move {
                if let Err(error) = follow(&control, &id).await {
                    tracing::debug!(%error, host = id, "synthetic JVM stopped");
                }
            });
        }
        Ok(self.connection(id).map_or(Progress::Pending, |connection| Progress::Ready(Box::new(connection))))
    }

    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        lock(&self.registered).contains(id).then(|| RuntimeConnection {
            token: credential(id),
            identity: identity(id),
            player_endpoint: "127.0.0.1:1".into(),
        })
    }

    fn register(&self, token: &str, registration: ProcessRegistration) -> chunk_control::Result<ProcessIdentity> {
        let registered = registration.identity.unwrap_or_default();
        let id = registered.runtime_id.clone();
        if token != format!("Bearer {}", credential(&id)) || registered != identity(&id) {
            return Err(chunk_control::Error::Invalid("unknown synthetic runtime"));
        }
        lock(&self.registered).insert(id);
        Ok(registered)
    }

    async fn release(&self, id: &str) -> chunk_control::Result<bool> {
        lock(&self.stopped).insert(id.into());
        Ok(true)
    }

    fn stopped(&self, id: &str) -> bool {
        lock(&self.stopped).contains(id)
    }
}

/// Plays host `id`'s JVM until its topic asks it to stop: registers, follows the topic and reports what changed.
async fn follow(control: &Arc<Control>, id: &str) -> Result<()> {
    let credential = credential(id);
    control.register_jvm(id, &credential, registration(id))?;
    let (mut topic, first) = Topic::open(control, id, STREAM)?;
    let mut jvm = Jvm::default();
    let (mut update, mut complete, mut health) = (Some(first), true, true);
    let mut interval = tokio::time::interval(HEALTH);
    loop {
        let mut report = JvmReport { complete, ..JvmReport::default() };
        let stop = match update {
            Some(update) => jvm.apply(&update, &mut report)?,
            None => false,
        };
        jvm.arrive(control, &mut report)?;
        if health {
            jvm.ticks += 100;
            report.health = Some(JvmHealth { ready: true, tick_count: jvm.ticks, ..JvmHealth::default() });
        }
        if complete || !(report.sessions.is_empty() && report.deliveries.is_empty() && report.health.is_none()) {
            control.report_jvm(id, &credential, STREAM, report)?;
        }
        if stop {
            return Ok(());
        }
        complete = false;
        tokio::select! {
            () = topic.changed() => health = false,
            _ = interval.tick() => health = true,
        }
        update = topic.update()?;
    }
}

/// A delivery a synthetic JVM holds, for the player it names.
struct Delivery {
    status: JvmDeliveryStatus,
    player: String,
}

#[derive(Default)]
struct Jvm {
    sessions: BTreeMap<String, JvmSessionStatus>,
    deliveries: BTreeMap<String, Delivery>,
    ticks: u64,
}

impl Jvm {
    /// Runs or ends the sessions the topic lists, prepares its deliveries and closes those withdrawn or left out,
    /// adding each change to `report`. Whether the topic asks the JVM to stop, which ends every session.
    fn apply(&mut self, update: &sync::Update, report: &mut JvmReport) -> Result<bool> {
        let stop = update.upserts.iter().any(|entry| entry.key == "stop");
        let mut listed = BTreeSet::new();
        for entry in &update.upserts {
            let Some(sync::entry::State::Value(value)) = &entry.state else { continue };
            if let Some(id) = entry.key.strip_prefix("session/") {
                let wanted = JvmSession::decode(value.as_slice())?;
                let phase = if wanted.finish || stop { JvmSessionPhase::Ended } else { JvmSessionPhase::Ready };
                if self.sessions.get(id).is_none_or(|session| session.phase() != phase) {
                    let status = JvmSessionStatus {
                        id: id.into(),
                        session_type: wanted.session_type,
                        capacity: wanted.capacity,
                        phase: phase.into(),
                        ..JvmSessionStatus::default()
                    };
                    self.sessions.insert(id.into(), status.clone());
                    report.sessions.push(status);
                }
            } else if let Some(operation) = entry.key.strip_prefix("delivery/") {
                let wanted = JvmDelivery::decode(value.as_slice())?;
                listed.insert(operation);
                let held = self.deliveries.get(operation).map(|delivery| delivery.status.phase());
                let phase = match held {
                    None if !wanted.withdraw => JvmDeliveryPhase::Prepared,
                    Some(phase) if !wanted.withdraw => phase,
                    _ => JvmDeliveryPhase::Closed,
                };
                if held != Some(phase) {
                    let prepared = phase == JvmDeliveryPhase::Prepared;
                    let status = JvmDeliveryStatus {
                        operation_id: operation.into(),
                        generation: wanted.generation,
                        phase: phase.into(),
                        capability: if prepared { vec![42; 32] } else { Vec::new() },
                    };
                    let player = wanted.player.map(|player| player.uuid).unwrap_or_default();
                    self.deliveries.insert(operation.into(), Delivery { status: status.clone(), player });
                    report.deliveries.push(status);
                }
            }
        }
        // A delivery the topic leaves out is closed, then forgotten.
        self.deliveries.retain(|operation, delivery| {
            if listed.contains(operation.as_str()) {
                return true;
            }
            if delivery.status.phase() != JvmDeliveryPhase::Closed {
                delivery.status.phase = JvmDeliveryPhase::Closed.into();
                delivery.status.capability.clear();
                report.deliveries.push(delivery.status.clone());
            }
            false
        });
        Ok(stop)
    }

    /// Arrives each prepared delivery whose claim is activating, as when its player connects.
    fn arrive(&mut self, control: &Control, report: &mut JvmReport) -> Result<()> {
        let prepared = |delivery: &Delivery| delivery.status.phase() == JvmDeliveryPhase::Prepared;
        if !self.deliveries.values().any(prepared) {
            return Ok(());
        }
        let players = control.players()?.players;
        let activating: BTreeSet<_> = players
            .into_iter()
            .filter(|player| player.phase() == ClaimPhase::Activating)
            .filter_map(|player| player.identity.map(|identity| identity.uuid))
            .collect();
        for delivery in self.deliveries.values_mut().filter(|delivery| prepared(delivery)) {
            if activating.contains(&delivery.player) {
                delivery.status.phase = JvmDeliveryPhase::Arrived.into();
                delivery.status.capability.clear();
                report.deliveries.push(delivery.status.clone());
            }
        }
        Ok(())
    }
}
