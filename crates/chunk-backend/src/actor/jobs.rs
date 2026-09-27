use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chunk_contract::FunctionKind;
use chunk_js::{DeploymentId, Json, ScheduleIntent};
use chunk_store::{Job, JobCommand, JobIntent, JobState, Jobs};

use super::Actor;
use crate::{
    ActionHandle, ActionStatus, Error, Result,
    service::{Call, Request},
};

pub(super) struct Scheduled {
    pub snapshot: Jobs,
    active: BTreeMap<String, (u32, ActionHandle)>,
    ready: VecDeque<Job>,
    pending: bool,
    reply: Option<Request<Jobs>>,
}

impl Scheduled {
    pub fn new(snapshot: Jobs) -> Self {
        Self { snapshot, active: BTreeMap::new(), ready: VecDeque::new(), pending: false, reply: None }
    }

    /// Time until the earliest pending job becomes due. Jobs already due are
    /// dispatched as events arrive, so they never shorten the wait.
    pub fn next_due(&self) -> Option<Duration> {
        let now = now();
        self.snapshot
            .records
            .iter()
            .filter(|job| job.state == JobState::Pending && job.due_at > now)
            .map(|job| job.due_at - now)
            .min()
            .and_then(|millis| u64::try_from(millis).ok())
            .map(Duration::from_millis)
    }

    pub fn references(&self, deployment: &DeploymentId) -> bool {
        self.snapshot.records.iter().any(|job| job.deployment == deployment.as_str() && !job.state.terminal())
    }

    pub fn get(&self, id: &str, caller: &Json) -> Result<Job> {
        let caller: serde_json::Value = serde_json::from_str(caller.as_str())?;
        self.snapshot.records.iter().find(|job| job.id == id && job.caller == caller).cloned().ok_or(Error::Unknown)
    }
}

fn now() -> i64 {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()).unwrap_or(i64::MAX)
}

impl Actor {
    pub(super) fn scheduled_intents(
        &self,
        call: &Call,
        intents: Vec<ScheduleIntent>,
        timestamp: i64,
    ) -> Result<Vec<JobIntent>> {
        let caller: serde_json::Value = serde_json::from_str(call.caller.as_str())?;
        let due = |at: i64| {
            if at < 0 || at > timestamp.saturating_add(366 * 24 * 60 * 60 * 1000) {
                return Err(Error::Invalid("scheduled time must be within one year"));
            }
            Ok(at)
        };
        intents
            .into_iter()
            .map(|intent| {
                Ok(match intent {
                    ScheduleIntent::RunAt { id, at, function, arguments } => {
                        let mut target = Call { function, arguments: arguments.into(), ..call.clone() };
                        self.normalize_scoped_call(&mut target, true)?;
                        let deployment =
                            self.versions.get(&call.deployment).and_then(Option::as_ref).ok_or(Error::Contract)?;
                        if deployment
                            .functions
                            .get(&target.function)
                            .is_none_or(|function| function.kind != FunctionKind::Action)
                        {
                            return Err(Error::Contract);
                        }
                        JobIntent::Schedule(Job {
                            id: id.ok_or(Error::Invalid("missing job identity"))?,
                            deployment: call.deployment.as_str().into(),
                            function: target.function,
                            arguments: serde_json::from_str(target.arguments.as_str())?,
                            caller: caller.clone(),
                            due_at: due(at)?,
                            attempt: 1,
                            state: JobState::Pending,
                            result: None,
                        })
                    }
                    ScheduleIntent::Cancel { id } => JobIntent::Cancel { id, caller: caller.clone() },
                    ScheduleIntent::Retry { id, at, acknowledge_possible_effects } => {
                        JobIntent::Retry { id, caller: caller.clone(), due_at: due(at)?, acknowledge_possible_effects }
                    }
                })
            })
            .collect()
    }

    pub(super) fn job_control(&mut self, command: JobCommand, reply: Request<Jobs>) {
        if self.scheduled.pending {
            reply.finish(Err(Error::Busy));
            return;
        }
        if reply.cancellation.is_cancelled() {
            reply.finish(Err(Error::Cancelled));
            return;
        }
        match self.send(crate::commit::Job::Scheduling { command }) {
            Ok(()) => {
                self.scheduled.pending = true;
                self.scheduled.reply = Some(reply);
            }
            Err(error) => reply.finish(Err(error)),
        }
    }

    pub(super) fn scheduled(&mut self, command: JobCommand, result: Result<Jobs>) {
        self.scheduled.pending = false;
        match &result {
            Ok(snapshot) => {
                self.scheduled.snapshot = snapshot.clone();
                if let JobCommand::Claim { id, attempt, .. } = command
                    && let Some(job) = snapshot
                        .records
                        .iter()
                        .find(|job| job.id == id && job.attempt == attempt && job.state == JobState::Running)
                {
                    self.scheduled.ready.push_back(job.clone());
                }
            }
            Err(error) if !error.is_rejected_commit() => self.fail(&Error::CommitFailed),
            Err(_) => {}
        }
        if let Some(reply) = self.scheduled.reply.take() {
            reply.finish(result);
        }
    }

    pub(super) fn dispatch_jobs(&mut self) {
        let records = &self.scheduled.snapshot.records;
        self.scheduled.active.retain(|id, (attempt, handle)| {
            let live =
                records.iter().any(|job| &job.id == id && job.attempt == *attempt && job.state == JobState::Running);
            if !live {
                handle.cancel();
            }
            live
        });
        self.scheduled.ready.retain(|ready| {
            records
                .iter()
                .any(|job| job.id == ready.id && job.attempt == ready.attempt && job.state == JobState::Running)
        });
        if self.failure.is_some()
            || self.recovering
            || self.scheduled.pending
            || self.deploying.is_some()
            || self.releasing.is_some()
        {
            return;
        }
        let finished = self.scheduled.active.iter().find_map(|(id, (attempt, handle))| {
            let ActionStatus::Finished(outcome) = handle.status() else {
                return None;
            };
            let (state, result) = match outcome {
                Ok(json) if json.len() <= 64 * 1024 => (JobState::Succeeded, serde_json::from_str(&json).ok()),
                Ok(_) | Err(Error::Contract | Error::Json(_)) => (JobState::Failed, None),
                Err(Error::JavaScript(error))
                    if matches!(
                        error.as_ref(),
                        chunk_js::Error::JavaScript(_)
                            | chunk_js::Error::Invalid(_)
                            | chunk_js::Error::UnknownDeployment
                    ) =>
                {
                    (JobState::Failed, None)
                }
                Err(_) => (JobState::Unknown, None),
            };
            Some(JobCommand::Finish { id: id.clone(), attempt: *attempt, state, result })
        });
        if let Some(command) = finished {
            self.send_scheduling(command);
            return;
        }
        if let Some(job) = self.scheduled.ready.front().cloned() {
            let id = self.actions.job_id(&job);
            let result = DeploymentId::new(&job.deployment).map_err(Error::from).and_then(|deployment| {
                self.launch_action(
                    id,
                    Call {
                        deployment,
                        function: job.function.clone(),
                        arguments: job.arguments.clone().into(),
                        caller: job.caller.clone().into(),
                    },
                    Some(job.invocation_id()),
                    crate::commands::Purpose::Function,
                    true,
                    &chunk_js::Cancellation::default(),
                )
            });
            match result {
                Ok(handle) => {
                    self.scheduled.ready.pop_front();
                    self.scheduled.active.insert(job.id, (job.attempt, handle));
                }
                Err(Error::Busy | Error::Overloaded(_)) => {}
                Err(_) => {
                    if self.send_scheduling(JobCommand::Finish {
                        id: job.id,
                        attempt: job.attempt,
                        state: JobState::Unknown,
                        result: None,
                    }) {
                        self.scheduled.ready.pop_front();
                    }
                }
            }
            return;
        }
        // Jobs take at most a quarter of the live actions, leaving the rest to interactive work.
        if self.scheduled.active.len() >= (self.actions.limit / 4).max(1) || !self.actions.capacity() {
            return;
        }
        let now = now();
        if let Some(job) = self
            .scheduled
            .snapshot
            .records
            .iter()
            .filter(|job| job.state == JobState::Pending && job.due_at <= now)
            .min_by_key(|job| (job.due_at, &job.id))
        {
            self.send_scheduling(JobCommand::Claim { id: job.id.clone(), attempt: job.attempt, now });
        }
    }

    fn send_scheduling(&mut self, command: JobCommand) -> bool {
        match self.send(crate::commit::Job::Scheduling { command }) {
            Ok(()) => {
                self.scheduled.pending = true;
                true
            }
            Err(Error::Busy) => false,
            Err(_) => {
                self.fail(&Error::CommitFailed);
                false
            }
        }
    }
}
