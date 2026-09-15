use super::*;

pub(super) fn start(
    service: Service,
    invocation: String,
    mut input: tonic::Streaming<CommandClientFrame>,
) -> ReceiverStream<Result<CommandServerFrame, Status>> {
    let (sender, receiver) = mpsc::channel(4);
    tokio::spawn(async move {
        let accepted = CommandAccepted { invocation_id: invocation.clone(), status_only: false };
        if sender
            .send(Ok(CommandServerFrame { frame: Some(command_server_frame::Frame::Accepted(accepted)) }))
            .await
            .is_err()
        {
            return;
        }
        if invocation == "slow" || invocation == "follow" {
            service.waiting.fetch_add(1, Ordering::SeqCst);
            tokio::select! { () = service.release.notified() => {}, _ = input.message() => return }
        }
        let effects = match invocation.as_str() {
            "parallel" => {
                vec![(4, method("session_call")), (2, serde_json::json!({"kind":"message","text":"independent"}))]
            }
            "send_fail" | "send_ok" => vec![(3, method("session_send"))],
            _ => vec![(3, serde_json::json!({"kind":"message","text":"done"}))],
        };
        let count = effects.len();
        for (sequence, request) in effects {
            let effect = CommandEffect {
                sequence,
                operation_id: format!("action/{invocation}/platform/{sequence}"),
                request_json: serde_json::to_vec(&request).unwrap(),
            };
            if sender
                .send(Ok(CommandServerFrame { frame: Some(command_server_frame::Frame::Effect(effect)) }))
                .await
                .is_err()
            {
                return;
            }
        }
        let mut failed = invocation == "send_fail";
        for _ in 0..count {
            let Ok(Some(CommandClientFrame { frame: Some(command_client_frame::Frame::Reply(reply)) })) =
                input.message().await
            else {
                return;
            };
            failed |= !reply.error.is_empty();
            service.reply_order.lock().unwrap().push(reply.sequence);
            if reply.error.is_empty() {
                service.replies.fetch_add(1, Ordering::SeqCst);
            }
        }
        let state = if failed { CommandCompletionState::Failed } else { CommandCompletionState::Succeeded };
        let _ = sender
            .send(Ok(CommandServerFrame {
                frame: Some(command_server_frame::Frame::Finished(CommandFinished {
                    state: i32::from(state),
                    result_json: b"null".to_vec(),
                    error: String::new(),
                })),
            }))
            .await;
    });
    ReceiverStream::new(receiver)
}
fn method(kind: &str) -> serde_json::Value {
    serde_json::json!({"kind":kind,"method":{"app":"lobby","session":"default","name":"population"},"arguments":{}})
}
