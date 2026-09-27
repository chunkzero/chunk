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

/// How long a session method effect may take.
const SESSION_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Performer {
    pub control: Arc<Control>,
    pub gateway: String,
    pub player: String,
    /// The claim the command was started under.
    pub origin: ArrivedClaim,
    /// Whether the command's effects follow its player to other sessions.
    pub follow: bool,
    pub run: Arc<Run>,
}

impl Performer {
    /// Performs the command's effects until it finishes, then returns its outcome. The command is cancelled once
    /// nothing follows it, and its work still pending once it fails.
    pub async fn drive(self, mut handle: ActionHandle, mut effects: CommandEffects) -> chunk_backend::Result<Arc<str>> {
        let cancel = self.run.token();
        let mut open = true;
        let outcome = loop {
            tokio::select! {
                outcome = handle.outcome() => break outcome,
                effect = effects.recv(), if open => match effect {
                    Some(effect) => self.perform(effect),
                    None => open = false,
                },
                () = self.run.abandoned(), if !cancel.is_cancelled() => {
                    cancel.cancel();
                    handle.cancel();
                }
            }
        };
        if outcome.is_err() {
            cancel.cancel();
        }
        outcome
    }

    /// Performs `effect`, which fails once the player's claim is no longer the command's, or for a command that
    /// follows its player, once they left the connection it started on.
    fn perform(&self, effect: CommandEffect) {
        if effect.is_cancelled() || self.run.token().is_cancelled() {
            return effect.finish(None);
        }
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

    /// Calls session method `name` on the session the command started in, unless the command is cancelled first. A
    /// call's effect resolves with the method's result; a send's once the method is prepared, while it runs on and
    /// holds the effect's admission.
    fn session(&self, effect: CommandEffect, name: String, arguments: serde_json::Value, send: bool) {
        let (control, cancel, origin) = (self.control.clone(), self.run.token(), self.origin.identity.clone());
        tokio::spawn(async move {
            if cancel.is_cancelled() {
                return effect.finish(None);
            }
            let prepared = control
                .capture_session(&origin)
                .and_then(|captured| control.prepare_session_method(&captured, &name, arguments, SESSION_TIMEOUT));
            let Ok(prepared) = prepared else { return effect.finish(None) };
            let (call, _admission) = if send { (None, Some(effect.accept_running())) } else { (Some(effect), None) };
            let result = control.run_session_method(&prepared, &cancel.child_token(), &cancel).await;
            if let Some(effect) = call {
                let completed = result.phase == SessionMethodPhase::Completed as i32;
                effect.finish(completed.then_some(result.result_json.as_bytes()));
            }
        });
    }
}
