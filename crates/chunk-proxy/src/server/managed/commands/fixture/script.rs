//! The fake core's commands: the arrived player's catalog and suggestions, and commands that each hold one chat effect
//! until the gateway acknowledges it, then finish. `slow` and `follow` first wait for `release`. A command topic
//! follows one gateway stream, is stopped once a newer one opens, and cancels its command once the gateway leaves it.
//! One under an unused ID reserves it: it confirms with an empty snapshot and waits for the start, and closing it first
//! cancels the command, whose start then fails. A started command, and one whose topic a newer gateway stream stopped,
//! is cancelled unless followed within [`GRACE`].

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

/// A command, reserved or started.
#[derive(Clone)]
pub(in crate::server::managed::commands) struct Run {
    state: Arc<watch::Sender<Pending>>,
    cancel: CancellationToken,
    /// Subscriptions opened to the command's topic.
    opened: Arc<AtomicUsize>,
    /// Subscriptions following the command's topic now.
    following: Arc<AtomicUsize>,
}

impl Run {
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn started(&self) -> bool {
        self.state.borrow().started
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
    started: bool,
    effects: BTreeMap<u32, CommandEffect>,
    outcome: Option<CommandOutcome>,
}

impl Service {
    /// Runs command method `call` for the arrived player.
    pub(super) async fn command(&self, call: &CallRequest) -> Result<Vec<u8>, sync::Error> {
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
                let run = self.run(&call.operation_id);
                if !run.started() {
                    // The start outlives a dropped call, as core's does.
                    tokio::spawn(self.clone().start(run, input)).await.unwrap()?;
                }
                if self.reopening.load(Ordering::SeqCst) {
                    self.reopened.notified().await;
                    let reopen = self.reopen.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        reopen.notify_one();
                    });
                }
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

    /// The command under `operation`, or a new reservation of it.
    fn run(&self, operation: &str) -> Run {
        let mut runs = self.runs.lock().unwrap();
        let run = runs.entry(operation.to_owned()).or_insert_with(|| Run {
            state: Arc::new(watch::Sender::new(Pending::default())),
            cancel: self.watches.child_token(),
            opened: Arc::default(),
            following: Arc::default(),
        });
        run.clone()
    }

    /// Admits the command, first waiting for `admit` while `admission` holds starts, unless it's cancelled first.
    async fn start(self, run: Run, input: String) -> Result<(), sync::Error> {
        let stopped = || sync::Error { code: Code::Stopped.into(), message: "closed before it started".into() };
        if self.admission.load(Ordering::SeqCst) {
            self.queued.fetch_add(1, Ordering::SeqCst);
            tokio::select! {
                () = run.cancel.cancelled() => return Err(stopped()),
                () = self.admit.notified() => {}
            }
        }
        if run.cancel.is_cancelled() {
            return Err(stopped());
        }
        run.state.send_modify(|pending| pending.started = true);
        if run.following.load(Ordering::SeqCst) == 0 {
            run.orphan();
        }
        tokio::spawn(self.script(run, input));
        Ok(())
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

    /// Follows the command under `operation` from the gateway stream `arguments` names, reserving it if unused.
    pub(super) fn follow(&self, operation: &str, arguments: &[u8]) -> ReceiverStream<Result<Update, Status>> {
        let stream = CommandSubscription::decode(arguments).unwrap().stream;
        let run = self.run(operation);
        run.opened.fetch_add(1, Ordering::SeqCst);
        run.following.fetch_add(1, Ordering::SeqCst);
        self.follows.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel(1);
        let service = self.clone();
        tokio::spawn(async move {
            let followed = async {
                let mut state = run.state.subscribe();
                if !run.started() {
                    let _ = sender.send(Ok(Update { snapshot: true, ..Update::default() })).await;
                    tokio::select! {
                        () = sender.closed() => return Ended::Left,
                        _ = state.wait_for(|pending| pending.started) => {}
                    }
                }
                let mut first = true;
                loop {
                    if *service.stream.lock().unwrap() != stream {
                        let error = sync::Error { code: Code::Stopped.into(), message: "superseded".into() };
                        let _ = sender.send(Ok(Update { error: Some(error), ..Update::default() })).await;
                        return Ended::Superseded;
                    }
                    if std::mem::take(&mut first) || state.has_changed().unwrap_or(false) {
                        let (update, last) = snapshot(&state.borrow_and_update());
                        if sender.send(Ok(update)).await.is_err() {
                            return Ended::Left;
                        }
                        if last {
                            return Ended::Finished;
                        }
                    }
                    tokio::select! {
                        () = sender.closed() => return Ended::Left,
                        () = tokio::time::sleep(Duration::from_millis(5)) => {}
                    }
                }
            };
            let ended = followed.await;
            run.following.fetch_sub(1, Ordering::SeqCst);
            match ended {
                Ended::Superseded => run.orphan(),
                Ended::Left => run.cancel.cancel(),
                Ended::Finished => {}
            }
        });
        ReceiverStream::new(receiver)
    }
}

/// How a command's subscription ended.
enum Ended {
    Finished,
    /// A newer gateway stream stopped it, so the gateway may follow the command again.
    Superseded,
    /// The gateway left it.
    Left,
}

fn snapshot(pending: &Pending) -> (Update, bool) {
    let entry = |key: String, value: Vec<u8>| sync::Entry { key, state: Some(sync::entry::State::Value(value.into())) };
    let upserts = if let Some(outcome) = &pending.outcome {
        vec![entry("outcome".into(), outcome.encode_to_vec())]
    } else {
        let effects = pending.effects.iter();
        effects.map(|(sequence, effect)| entry(sequence.to_string(), effect.encode_to_vec())).collect()
    };
    (Update { snapshot: true, upserts, ..Update::default() }, pending.outcome.is_some())
}
