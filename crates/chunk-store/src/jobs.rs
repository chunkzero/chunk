use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
    Cancelled,
}
impl JobState {
    #[must_use]
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub id: String,
    pub deployment: String,
    pub function: String,
    pub arguments: Value,
    pub caller: Value,
    pub due_at: i64,
    pub attempt: u32,
    pub state: JobState,
    pub result: Option<Value>,
}
impl Job {
    #[must_use]
    pub fn invocation_id(&self) -> String {
        format!("job/{}/attempt/{}", self.id, self.attempt)
    }
}

#[derive(Clone, Debug)]
pub enum JobIntent {
    Schedule(Job),
    Cancel { id: String, caller: Value },
    Retry { id: String, caller: Value, due_at: i64, acknowledge_possible_effects: bool },
}

/// A hosted adapter durably installs this generation's alarm before acknowledging.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WakeHandoff {
    pub generation: u64,
    pub due_at: Option<i64>,
    pub acknowledged: bool,
    /// Claimed attempts must drain before a host suspends the backend.
    pub running: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Jobs {
    pub records: Vec<Job>,
    pub wake: WakeHandoff,
}

#[derive(Clone, Debug)]
pub enum JobCommand {
    Recover,
    Claim {
        id: String,
        attempt: u32,
        now: i64,
    },
    Finish {
        id: String,
        attempt: u32,
        state: JobState,
        result: Option<Value>,
    },
    Forget {
        id: String,
        caller: Value,
    },
    /// Cancels the deployment's pending jobs and marks its running ones unknown.
    CancelDeployment {
        deployment: String,
    },
    AcknowledgeWake {
        generation: u64,
        due_at: Option<i64>,
    },
}
