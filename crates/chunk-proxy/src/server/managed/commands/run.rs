use super::{Tasks, backend, effects, scope::Origin};
use crate::server::transport::invalid_data;
use chunk_proto::v1::{
    CommandClientFrame, CommandCompletionState, CommandEffectReply, CommandStart, command_client_frame,
    command_server_frame,
};
use std::{collections::BTreeSet, io};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_stream::wrappers::ReceiverStream;

pub(super) async fn execute(tasks: &Tasks, origin: &Origin, follow: bool, invocation: &str) -> io::Result<()> {
    let mut tasks = tasks.clone();
    tasks.invocation = tokio_util::sync::CancellationToken::new();
    let abort = tasks.invocation.clone().drop_guard();
    let (sender, receiver) = mpsc::channel(4);
    sender
        .send(CommandClientFrame {
            frame: Some(command_client_frame::Frame::Start(CommandStart { invocation_id: invocation.into() })),
        })
        .await
        .map_err(io::Error::other)?;
    let mut request = backend::authenticated(&tasks.platform, ReceiverStream::new(receiver))?;
    request.set_timeout(std::time::Duration::from_secs(60));
    let mut stream = backend::client(&tasks.platform).run(request).await.map_err(io::Error::other)?.into_inner();
    let mut accepted = false;
    let mut seen = BTreeSet::new();
    let mut pending = JoinSet::new();
    loop {
        tokio::select! {
            result = pending.join_next(), if !pending.is_empty() => {
                let (sequence, result) = result.ok_or_else(|| invalid_data("missing command effect"))?.map_err(io::Error::other)?;
                reply(&sender, sequence, result).await?;
            }
            frame = stream.message() => {
                let frame = frame.map_err(io::Error::other)?.ok_or_else(|| invalid_data("command outcome unknown"))?;
                match frame.frame {
                    Some(command_server_frame::Frame::Accepted(receipt)) if !accepted && receipt.invocation_id == invocation && !receipt.status_only => { accepted = true; }
                    Some(command_server_frame::Frame::Effect(effect)) if accepted => {
                        if !(1..=256).contains(&effect.sequence) || !seen.insert(effect.sequence)
                            || effect.operation_id != format!("action/{invocation}/platform/{}", effect.sequence) {
                            return Err(invalid_data("invalid command effect identity"));
                        }
                        let Ok(permit) = tasks.effects.clone().try_acquire_owned() else {
                            reply(&sender, effect.sequence, Err(invalid_data("command effect capacity exhausted"))).await?;
                            continue;
                        };
                        let tasks = tasks.clone();
                        let origin = origin.clone();
                        let cleanup = tasks.platform.cleanup.clone();
                        pending.spawn(cleanup.track_future(async move {
                            let _permit = permit;
                            (effect.sequence, effects::apply(&tasks, &origin, follow, &effect).await)
                        }));
                    }
                    Some(command_server_frame::Frame::Finished(result)) if accepted => {
                        return if result.state == i32::from(CommandCompletionState::Succeeded) && pending.is_empty() { abort.disarm(); Ok(()) }
                        else { Err(invalid_data("command failed or outcome unknown")) };
                    }
                    _ => return Err(invalid_data("invalid command stream state")),
                }
            }
        }
    }
}
async fn reply(
    sender: &mpsc::Sender<CommandClientFrame>,
    sequence: u32,
    result: io::Result<serde_json::Value>,
) -> io::Result<()> {
    let (result_json, error) = match result {
        Ok(result) => (serde_json::to_vec(&result).map_err(invalid_data)?, String::new()),
        Err(_) => (Vec::new(), "Effect unavailable or outcome unknown".into()),
    };
    sender
        .send(CommandClientFrame {
            frame: Some(command_client_frame::Frame::Reply(CommandEffectReply { sequence, result_json, error })),
        })
        .await
        .map_err(io::Error::other)
}
