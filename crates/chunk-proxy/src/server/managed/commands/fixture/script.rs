//! The fake core's commands: the arrived player's catalog and suggestions, and commands that each hold one chat effect
//! until the gateway acknowledges it, then finish. `slow` and `follow` first wait for `release`. A command topic
//! follows one gateway stream, is stopped once a newer one opens, and cancels its command once the gateway leaves it.
//! A started command, and one whose topic a newer gateway stream stopped, is cancelled unless followed within
//! [`GRACE`].

use super::Service;
use chunk_proto::sync::v1::{
    self as sync, CallRequest, CommandArguments, CommandEffect, CommandOutcome, CommandStarted, CommandSubscription,
    CommandsResult, EffectArguments, EffectResult, SuggestResult, Update, command_effect, command_outcome, error::Code,
};
use prost::Message;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::Status;

/// How long a command waits for a subscription.
const GRACE: Duration = Duration::from_millis(500);

/// A started command.
#[derive(Clone)]
pub(in crate::server::managed::commands) struct Run {
    state: Arc<watch::Sender<Pending>>,
    cancel: CancellationToken,
    /// Subscriptions opened to the command's topic.
    opened: Arc<AtomicUsize>,
}

impl Run {
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Cancels the command unless a subscription opens within [`GRACE`].
    fn orphan(&self) {
        let (run, opened) = (self.clone(), self.opened.load(Ordering::SeqCst));
        tokio::spawn(async move {
            tokio::time::sleep(GRACE).await;
            if run.opened.load(Ordering::SeqCst) == opened {
                run.cancel.cancel();
            }
        });
    }
}

#[derive(Default)]
struct Pending {
    effects: BTreeMap<u32, CommandEffect>,
    outcome: Option<CommandOutcome>,
}

impl Service {
    /// Runs command method `call` for the arrived player.
    pub(super) fn command(&self, call: &CallRequest) -> Result<Vec<u8>, sync::Error> {
        let player = call.caller.as_ref().map(|caller| caller.player.as_str());
        match call.method.as_str() {
            "chunk:commands" => {
                assert_eq!(player, Some("player"));
                if self.catalog_unavailable.load(Ordering::SeqCst) {
                    return Err(sync::Error { code: Code::Unavailable.into(), message: "commit pending".into() });
                }
                let allowed =
                    if self.allowed.load(Ordering::SeqCst) { self.commands.keys().cloned().collect() } else { vec![] };
                let commands_json = serde_json::to_vec(&self.commands).unwrap();
                Ok(CommandsResult { commands_json, allowed }.encode_to_vec())
            }
            "chunk:suggest" => {
                assert_eq!(player, Some("player"));
                Ok(SuggestResult { values: vec!["alpha".into(), "alpine".into(), "beta".into()] }.encode_to_vec())
            }
            "chunk:command" => {
                assert_eq!(player, Some("player"));
                assert!(call.operation_id.starts_with("prep:"));
                let input = CommandArguments::decode(call.arguments.as_slice()).unwrap().input;
                let run = Run {
                    state: Arc::new(watch::Sender::new(Pending::default())),
                    cancel: self.watches.child_token(),
                    opened: Arc::default(),
                };
                self.runs.lock().unwrap().insert(call.operation_id.clone(), run.clone());
                run.orphan();
                tokio::spawn(self.clone().script(run, input));
                Ok(CommandStarted {}.encode_to_vec())
            }
            "chunk:effect" => {
                assert!(call.caller.is_none() && call.operation_id.is_empty());
                let EffectArguments { operation_id, sequence, failed } =
                    EffectArguments::decode(call.arguments.as_slice()).unwrap();
                let run = self.runs.lock().unwrap().get(&operation_id).cloned();
                let acknowledged = run.is_some_and(|run| {
                    run.state.send_if_modified(|pending| pending.effects.remove(&sequence).is_some())
                });
                if acknowledged && !failed {
                    self.replies.fetch_add(1, Ordering::SeqCst);
                }
                Ok(EffectResult { unknown: !acknowledged }.encode_to_vec())
            }
            _ => unreachable!(),
        }
    }

    async fn script(self, run: Run, input: String) {
        let work = async {
            if input == "slow" || input == "follow" {
                self.waiting.fetch_add(1, Ordering::SeqCst);
                self.release.notified().await;
            }
            let effect = CommandEffect { effect: Some(command_effect::Effect::Message("done".into())) };
            run.state.send_modify(|pending| _ = pending.effects.insert(3, effect));
            if self.outstanding.load(Ordering::SeqCst) {
                self.release.notified().await;
            } else {
                let _ = run.state.subscribe().wait_for(|pending| pending.effects.is_empty()).await;
            }
            let outcome = CommandOutcome { outcome: Some(command_outcome::Outcome::ResultJson(b"null".to_vec())) };
            run.state.send_modify(|pending| pending.outcome = Some(outcome));
        };
        tokio::select! { () = run.cancel.cancelled() => {}, () = work => {} }
    }

    /// Follows the command started under `operation` from the gateway stream `arguments` names.
    pub(super) fn follow(&self, operation: &str, arguments: &[u8]) -> ReceiverStream<Result<Update, Status>> {
        let stream = CommandSubscription::decode(arguments).unwrap().stream;
        let run = self.runs.lock().unwrap().get(operation).cloned().expect("the command started");
        run.opened.fetch_add(1, Ordering::SeqCst);
        self.follows.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel(1);
        let service = self.clone();
        tokio::spawn(async move {
            let mut state = run.state.subscribe();
            let mut first = true;
            loop {
                if *service.stream.lock().unwrap() != stream {
                    let error = sync::Error { code: Code::Stopped.into(), message: "superseded".into() };
                    let _ = sender.send(Ok(Update { error: Some(error), ..Update::default() })).await;
                    return run.orphan();
                }
                if std::mem::take(&mut first) || state.has_changed().unwrap_or(false) {
                    let (update, last) = snapshot(&state.borrow_and_update());
                    if sender.send(Ok(update)).await.is_err() {
                        break;
                    }
                    if last {
                        return;
                    }
                }
                tokio::select! {
                    () = sender.closed() => break,
                    () = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
            run.cancel.cancel();
        });
        ReceiverStream::new(receiver)
    }
}

fn snapshot(pending: &Pending) -> (Update, bool) {
    let entry = |key: String, value: Vec<u8>| sync::Entry { key, state: Some(sync::entry::State::Value(value)) };
    let upserts = if let Some(outcome) = &pending.outcome {
        vec![entry("outcome".into(), outcome.encode_to_vec())]
    } else {
        let effects = pending.effects.iter();
        effects.map(|(sequence, effect)| entry(sequence.to_string(), effect.encode_to_vec())).collect()
    };
    (Update { snapshot: true, upserts, ..Update::default() }, pending.outcome.is_some())
}
