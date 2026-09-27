//! The caller app code receives, derived from the credential's class and checked against control's state.

use super::{
    auth::{Class, Credentials, Principal},
    errors,
};
use chunk_control::{Control, Generation};
use chunk_proto::sync::v1::{Caller, Error, error::Code};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::watch;

/// The authority a stream was opened with, which must still hold whenever it sends.
pub(super) struct Grant {
    credentials: Arc<Credentials>,
    principal: Principal,
    deployment: String,
    caller: Option<Caller>,
}

impl Grant {
    pub fn new(credentials: Arc<Credentials>, principal: Principal, deployment: &str, caller: Option<&Caller>) -> Self {
        Self { credentials, principal, deployment: deployment.to_owned(), caller: caller.cloned() }
    }

    /// The caller app code receives, if the credential still grants it under control's current state.
    pub fn check(&self) -> Result<chunk_js::Json, Error> {
        if self.credentials.class(&self.principal.credential).as_ref() != Some(&self.principal.class) {
            return Err(errors::error(Code::Stopped, "the credential's process stopped"));
        }
        derive(&self.credentials.control, &self.principal.class, &self.deployment, self.caller.as_ref())
    }

    /// Changes whenever control's state does.
    pub fn changes(&self) -> watch::Receiver<Generation> {
        self.credentials.control.subscribe()
    }
}

/// The caller for `class` acting as `caller` in `deployment`. A gateway may name a player it holds a claim for; a
/// JVM must name a session its host runs in `deployment` and may name a player whose current claim is delivered to it,
/// from reservation until the delivery closes; the CLI names none.
pub(super) fn derive(
    control: &Control,
    class: &Class,
    deployment: &str,
    caller: Option<&Caller>,
) -> Result<chunk_js::Json, Error> {
    let (session, player) = caller.map_or(("", ""), |caller| (caller.session.as_str(), caller.player.as_str()));
    let value = match class {
        Class::Gateway { id } => {
            if !session.is_empty() {
                return Err(errors::denied("a gateway names no session"));
            }
            if player.is_empty() {
                json!({"kind": "gateway"})
            } else if control.holds_claim(id, player).map_err(|failure| errors::control(&failure))? {
                json!({"kind": "gateway", "player": player})
            } else {
                return Err(errors::denied("the gateway holds no claim for this player"));
            }
        }
        Class::Jvm { host } => {
            if session.is_empty() {
                return Err(errors::denied("a JVM names a session its host runs"));
            }
            let named = (!player.is_empty()).then_some(player);
            let scope = control.session_scope(host, session, named).map_err(|failure| errors::control(&failure))?;
            if scope.deployment != deployment {
                return Err(errors::denied("the session runs another deployment"));
            }
            let mut value = json!({"session": session, "app": scope.app});
            if let Some(player) = named {
                value["player"] = player.into();
            }
            value
        }
        Class::Cli => {
            if !session.is_empty() || !player.is_empty() {
                return Err(errors::denied("the CLI names no caller"));
            }
            json!({"kind": "cli"})
        }
    };
    Ok(value.into())
}
