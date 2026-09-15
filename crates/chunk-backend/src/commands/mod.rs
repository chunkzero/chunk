use std::sync::Arc;

use chunk_contract::{Deployment, Schema};
use chunk_js::{DeploymentId, Json};
use chunk_proto::v1::CommandScope;
use tokio::sync::mpsc;

use crate::{Call, Result, service::Request};

mod effects;
mod transport;
pub use transport::CommandService;

#[derive(Clone)]
pub(crate) enum Purpose {
    Function,
    Hook,
    Command(Arc<CommandBinding>),
}

impl Purpose {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Hook => "hook",
            Self::Command(_) => "command",
        }
    }
    pub fn command(self) -> Option<Arc<CommandBinding>> {
        if let Self::Command(binding) = self { Some(binding) } else { None }
    }
}

#[derive(Clone)]
pub(crate) struct CommandBinding {
    pub scope: CommandScope,
    pub input: String,
    pub effects: mpsc::Sender<PlatformEffect>,
}

pub(crate) struct PlatformEffect {
    pub sequence: u32,
    pub request: Json,
    pub result: Schema,
    pub receipt: bool,
    pub reply: Request<Arc<str>>,
}

#[derive(Clone)]
pub(crate) struct Prepared {
    pub deployment: DeploymentId,
    pub scope: CommandScope,
    pub command: String,
    pub input: String,
    pub follow_player: bool,
}

impl Prepared {
    pub fn call(&self) -> Call {
        Call {
            deployment: self.deployment.clone(),
            function: self.command.clone(),
            arguments: serde_json::Value::Null.into(),
            caller: serde_json::Value::Null.into(),
        }
    }
}

pub(crate) fn validate_effect(
    deployment: &Deployment,
    binding: &CommandBinding,
    request: &Json,
) -> Result<(Json, Schema, bool)> {
    effects::validate(deployment, &binding.scope, request)
}

#[cfg(test)]
mod tests;
