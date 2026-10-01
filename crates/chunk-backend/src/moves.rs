//! The moves an action asks for with `ctx.routing.move`, which the backend's host serves through [`PlayerMoves`].

use std::sync::{Arc, RwLock};

use chunk_contract::{EffectDestination, MoveRefusal};
use chunk_js::Json;
use serde::Deserialize;

/// Moves players for actions. Core serves it with control's moves.
pub trait PlayerMoves: Send + Sync {
    /// Asks to move `player` to `destination` through its admission policy, under `operation`, which names the move's
    /// destination claim, and returns why it was refused, if it was.
    /// # Errors
    /// Reports a move that failed without a refusal, such as one core can't take now.
    fn move_player(
        &self,
        operation: String,
        player: String,
        destination: EffectDestination,
    ) -> Result<Option<MoveRefusal>, String>;
}

/// Where the environment's current [`PlayerMoves`] is installed.
pub(crate) type Slot = Arc<RwLock<Option<Arc<dyn PlayerMoves>>>>;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Move { player: String, destination: EffectDestination },
}

/// Performs an action's platform `request` through `moves` under `operation`, returning its JSON result.
pub(crate) fn perform(moves: &Slot, operation: &str, request: &Json) -> Result<String, String> {
    let Request::Move { player, destination } =
        serde_json::from_str(request.as_str()).map_err(|_| "Invalid platform request")?;
    for value in [&player, &destination.key, &destination.session_type, &destination.machine_profile] {
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err("Invalid move request".into());
        }
    }
    let moves = moves.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let moves = moves.ok_or("Moves unavailable")?;
    let outcome = match moves.move_player(operation.to_owned(), player, destination)? {
        None => serde_json::json!({"state": "accepted", "operationId": operation}),
        Some(reason) => serde_json::json!({"state": "refused", "reason": reason}),
    };
    Ok(outcome.to_string())
}
