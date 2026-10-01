//! Moves of players that gameplay asks for: an action's, which the backend hands to core, and a JVM's `chunk:move`.
//! Both are control's moves, through the destination's admission policy, which the player's gateway carries out.

use chunk_backend::PlayerMoves;
use chunk_contract::{EffectDestination, MoveRefusal};
use chunk_control::{Control, MoveRequest};
use chunk_proto::{control::v1::SessionDemand, sync::v1 as sync};
use std::sync::Weak;

/// Serves the backend's action moves, which may move any player in the environment, while control runs.
pub(super) struct ActionMoves(pub Weak<Control>);

impl PlayerMoves for ActionMoves {
    fn move_player(
        &self,
        operation: String,
        player: String,
        destination: EffectDestination,
    ) -> Result<Option<MoveRefusal>, String> {
        let demand = SessionDemand {
            key: destination.key,
            session_type: destination.session_type,
            machine_profile: destination.machine_profile,
        };
        let request = MoveRequest { operation_id: operation, player_id: player, demand, source: None };
        let control = self.0.upgrade().ok_or("control stopped")?;
        match control.move_player(request) {
            Ok(_) => Ok(None),
            Err(chunk_control::Error::Refused(refusal)) => Ok(Some(refusal)),
            Err(failure) => Err(failure.to_string()),
        }
    }
}

/// `refusal` on the wire.
pub(super) fn refusal(refusal: MoveRefusal) -> sync::MoveRefusal {
    match refusal {
        MoveRefusal::Offline => sync::MoveRefusal::Offline,
        MoveRefusal::Stale => sync::MoveRefusal::Stale,
        MoveRefusal::Full => sync::MoveRefusal::Full,
        MoveRefusal::UnknownDestination => sync::MoveRefusal::UnknownDestination,
    }
}
