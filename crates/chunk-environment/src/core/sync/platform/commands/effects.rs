//! The effects a running command asks for. Core performs `enter` and session method effects itself, through control's
//! moves and session methods, and holds chat, action bar and title effects on the command's topic for its gateway.

use super::super::super::runs::Run;
use chunk_backend::{ActionHandle, CommandEffect, CommandEffects};
use chunk_contract::{Effect, EffectDestination};
use chunk_control::{ArrivedClaim, Control};
use chunk_proto::{
    sync::v1::{self as sync, CommandTitle, command_effect},
    v1::{MovePlayerRequest, SessionDemand, SessionMethodPhase},
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

/// How long a session method effect may take.
const SESSION_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Performer {
    pub control: Arc<Control>,
    /// Fires when core stops, cancelling session methods still running.
    pub stop: CancellationToken,
    pub gateway: String,
    pub player: String,
    /// The claim the command was started under.
    pub origin: ArrivedClaim,
    /// Whether the command's effects follow its player to other sessions.
    pub follow: bool,
    pub run: Arc<Run>,
}

impl Performer {
    /// Performs the command's effects until it finishes, then returns its outcome.
    pub async fn drive(self, mut handle: ActionHandle, mut effects: CommandEffects) -> chunk_backend::Result<Arc<str>> {
        let mut open = true;
        let outcome = loop {
            tokio::select! {
                outcome = handle.outcome() => break outcome,
                effect = effects.recv(), if open => match effect {
                    Some(effect) => self.perform(effect),
                    None => open = false,
                },
            }
        };
        self.run.finish();
        outcome
    }

    /// Performs `effect`, which fails once the player's claim is no longer the command's, or for a command that
    /// follows its player, once they left the connection it started on.
    fn perform(&self, effect: CommandEffect) {
        let Ok(request) = serde_json::from_str::<Effect>(effect.request().as_str()) else {
            return effect.finish(None);
        };
        let current = self.control.arrived_claim(&self.gateway, &self.player).ok().filter(|current| {
            current.request.connection_id == self.origin.request.connection_id
                && (self.follow || current.identity == self.origin.identity)
        });
        let Some(current) = current else { return effect.finish(None) };
        let packet = match request {
            Effect::Message { text } => command_effect::Effect::Message(text),
            Effect::ActionBar { text } => command_effect::Effect::ActionBar(text),
            Effect::Title { title, subtitle } => command_effect::Effect::Title(CommandTitle { title, subtitle }),
            Effect::Enter { destination } => return self.enter(effect, current, destination),
            Effect::SessionCall { method, arguments } => return self.session(effect, method.name, arguments, false),
            Effect::SessionSend { method, arguments } => return self.session(effect, method.name, arguments, true),
        };
        self.run.publish(effect, sync::CommandEffect { effect: Some(packet) });
    }

    /// Queues the player's move from their `current` claim, which the gateway carries out as control's move.
    fn enter(&self, effect: CommandEffect, current: ArrivedClaim, destination: EffectDestination) {
        let request = MovePlayerRequest {
            operation_id: effect.operation_id(),
            player_id: self.player.clone(),
            demand: Some(SessionDemand {
                key: destination.key,
                session_type: destination.session_type,
                machine_profile: destination.machine_profile,
            }),
            expected_source: Some(current.identity),
            expected_connection_id: current.request.connection_id,
        };
        match self.control.move_player(request) {
            Ok(_) => effect.accept(),
            Err(_) => effect.finish(None),
        }
    }

    /// Calls session method `name` on the session the command started in. A call's effect resolves with the method's
    /// result; a send's once the method is prepared, while it runs on.
    fn session(&self, effect: CommandEffect, name: String, arguments: serde_json::Value, send: bool) {
        let (control, stop, origin) = (self.control.clone(), self.stop.clone(), self.origin.identity.clone());
        tokio::spawn(async move {
            let prepared = control
                .capture_session(&origin)
                .and_then(|captured| control.prepare_session_method(&captured, &name, arguments, SESSION_TIMEOUT));
            let Ok(prepared) = prepared else { return effect.finish(None) };
            let call = if send {
                effect.accept();
                None
            } else {
                Some(effect)
            };
            let result = control.run_session_method(&prepared, &stop.child_token(), &stop).await;
            if let Some(effect) = call {
                let completed = result.phase == SessionMethodPhase::Completed as i32;
                effect.finish(completed.then_some(result.result_json.as_bytes()));
            }
        });
    }
}
