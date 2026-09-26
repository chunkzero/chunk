//! The caller app code receives, derived from the credential's class and checked against control's state.

use super::{auth::Class, errors};
use chunk_control::Control;
use chunk_proto::sync::v1::{Caller, Error};
use serde_json::json;

/// The caller for `class` acting as `caller` in `deployment`. A gateway may name a player it holds a claim for; a
/// JVM must name a session its host runs in `deployment` and may name a player who arrived on it; the CLI names none.
pub(super) fn derive(
    control: &Control,
    class: &Class,
    deployment: &str,
    caller: Option<&Caller>,
) -> Result<chunk_js::Json, Error> {
    let (session, player) = caller.map_or(("", ""), |caller| (caller.session.as_str(), caller.player.as_str()));
    let value = match class {
        Class::Gateway => {
            if !session.is_empty() {
                return Err(errors::denied("a gateway names no session"));
            }
            if player.is_empty() {
                json!({"kind": "gateway"})
            } else if control.holds_claim(player).map_err(|failure| errors::control(&failure))? {
                json!({"kind": "gateway", "player": player})
            } else {
                return Err(errors::denied("no claim is held for this player"));
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
