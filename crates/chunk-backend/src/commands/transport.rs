use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use chunk_proto::v1::{self as wire, backend_commands_server, command_client_frame, command_server_frame};
use tokio::sync::{Mutex, Semaphore, mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

use super::{CommandBinding, PlatformEffect, Prepared, Purpose, scope_bytes};
use crate::{ActionId, Backend, Error, Result, Service, service::Command, transport::status};

#[derive(Clone, Default)]
struct Progress {
    accepted: bool,
    finished: Option<wire::CommandFinished>,
}
struct Entry {
    id: ActionId,
    prepared: Prepared,
    created: Instant,
    started: bool,
    progress: watch::Sender<Progress>,
    cancel: CancellationToken,
}

/// A separate platform credential is required for discovery, authorization and execution.
#[derive(Clone)]
pub struct CommandService {
    backend: Backend,
    platform: Service,
    entries: Arc<Mutex<BTreeMap<String, Entry>>>,
    streams: Arc<Semaphore>,
}

impl CommandService {
    /// # Errors
    /// Requires valid and distinct application/platform credentials.
    pub fn new(backend: Backend, application: &str, platform: &str) -> Result<Self> {
        if application == platform {
            return Err(Error::Invalid("command authority must differ from application authority"));
        }
        Service::new(backend.clone(), application)?;
        Ok(Self {
            platform: Service::new(backend.clone(), platform)?,
            backend,
            entries: Arc::default(),
            streams: Arc::new(Semaphore::new(16)),
        })
    }
    #[must_use]
    pub fn into_server(self) -> backend_commands_server::BackendCommandsServer<Self> {
        backend_commands_server::BackendCommandsServer::new(self)
            .max_decoding_message_size(128 * 1024)
            .max_encoding_message_size(2 * 1024 * 1024)
    }
    pub(crate) fn workers(&self) -> tokio_util::task::TaskTracker {
        self.platform.workers()
    }
    pub(crate) fn shutdown(&self) -> CancellationToken {
        self.platform.shutdown()
    }
}

#[tonic::async_trait]
impl backend_commands_server::BackendCommands for CommandService {
    async fn catalog(
        &self,
        request: Request<wire::CommandScope>,
    ) -> std::result::Result<Response<wire::CommandCatalog>, Status> {
        let id = self.platform.binding(&request)?;
        let scope = request.into_inner();
        let bytes = scope_bytes(&scope);
        let catalog = bounded(self.backend.submit_sized(bytes, |reply| Command::Catalog { id, scope, reply })).await?;
        Ok(Response::new(catalog))
    }
    async fn suggest(
        &self,
        request: Request<wire::CommandSuggestionRequest>,
    ) -> std::result::Result<Response<wire::CommandSuggestionResult>, Status> {
        let id = self.platform.binding(&request)?;
        let request = request.into_inner();
        let bytes = request.scope.as_ref().map_or(0, scope_bytes)
            + request.command_id.len()
            + request.query.len()
            + request.input.len();
        let suggestions =
            bounded(self.backend.submit_sized(bytes, |reply| Command::Suggest { id, request, reply })).await?;
        Ok(Response::new(suggestions))
    }
    async fn prepare(
        &self,
        request: Request<wire::PrepareCommand>,
    ) -> std::result::Result<Response<wire::PreparedCommand>, Status> {
        let id = self.platform.binding(&request)?;
        let message = request.into_inner();
        let scope = message.scope.ok_or_else(|| Status::invalid_argument("missing command scope"))?;
        let bytes = scope_bytes(&scope) + message.command_id.len() + message.input.len();
        let prepared = bounded(self.backend.submit_sized(bytes, |reply| Command::Prepare {
            id,
            scope,
            command: message.command_id,
            input: message.input,
            reply,
        }))
        .await?;
        let id = self.backend.allocate_action_id().await.map_err(|error| status(&error))?;
        let invocation_id = id.to_string();
        let follow_player = prepared.follow_player;
        let mut entries = self.entries.lock().await;
        entries.retain(|_, entry| {
            entry.created.elapsed() < Duration::from_secs(60)
                || (entry.started && entry.progress.borrow().finished.is_none())
        });
        if entries.len() >= 64 {
            let oldest = entries
                .iter()
                .filter(|(_, entry)| !entry.started || entry.progress.borrow().finished.is_some())
                .min_by_key(|(_, entry)| entry.created)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            } else {
                return Err(Status::resource_exhausted("command invocation capacity"));
            }
        }
        let (progress, _) = watch::channel(Progress::default());
        entries.insert(
            invocation_id.clone(),
            Entry { id, prepared, created: Instant::now(), started: false, progress, cancel: CancellationToken::new() },
        );
        Ok(Response::new(wire::PreparedCommand { invocation_id, follow_player }))
    }

    type RunStream = ReceiverStream<std::result::Result<wire::CommandServerFrame, Status>>;
    async fn run(
        &self,
        request: Request<tonic::Streaming<wire::CommandClientFrame>>,
    ) -> std::result::Result<Response<Self::RunStream>, Status> {
        let deployment = self.platform.binding(&request)?;
        let permit = self
            .streams
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("command stream capacity"))?;
        let mut incoming = request.into_inner();
        let initial = tokio::time::timeout(Duration::from_secs(5), incoming.message())
            .await
            .map_err(|_| Status::deadline_exceeded("command start deadline"))??;
        let Some(wire::CommandClientFrame { frame: Some(command_client_frame::Frame::Start(start)) }) = initial else {
            return Err(Status::invalid_argument("first command frame must start a prepared invocation"));
        };
        let (owner, id, prepared, progress, cancel) = {
            let mut entries = self.entries.lock().await;
            let entry = entries
                .get_mut(&start.invocation_id)
                .filter(|entry| {
                    entry.prepared.deployment == deployment
                        && (entry.started || entry.created.elapsed() < Duration::from_secs(60))
                })
                .ok_or_else(|| Status::not_found("command outcome unknown; do not start a replacement invocation"))?;
            let owner = !entry.started;
            entry.started = true;
            (owner, entry.id.clone(), entry.prepared.clone(), entry.progress.clone(), entry.cancel.clone())
        };
        let (output, receiver) = mpsc::channel(8);
        let backend = self.backend.clone();
        let shutdown = self.shutdown();
        self.workers().spawn(async move {
            let _permit = permit;
            if owner {
                owner_run(backend, id, prepared, incoming, output, progress, cancel, shutdown).await;
            } else {
                observe(start.invocation_id, incoming, output, progress.subscribe(), cancel, shutdown).await;
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}

async fn bounded<T>(request: impl Future<Output = Result<T>>) -> std::result::Result<T, Status> {
    tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .map_err(|_| Status::deadline_exceeded("command authorization deadline"))?
        .map_err(|error| status(&error))
}

type Output = mpsc::Sender<std::result::Result<wire::CommandServerFrame, Status>>;
fn send(output: &Output, frame: command_server_frame::Frame) -> bool {
    output.try_send(Ok(wire::CommandServerFrame { frame: Some(frame) })).is_ok()
}
fn unknown() -> wire::CommandFinished {
    wire::CommandFinished {
        state: wire::CommandCompletionState::Unknown.into(),
        result_json: Vec::new(),
        error: "command interrupted; earlier effects may have completed".into(),
    }
}
fn finished(result: Result<Arc<str>>) -> wire::CommandFinished {
    match result {
        Ok(result) => wire::CommandFinished {
            state: wire::CommandCompletionState::Succeeded.into(),
            result_json: result.as_bytes().to_vec(),
            error: String::new(),
        },
        Err(Error::ActionOutcomeUnknown | Error::Cancelled | Error::Closed) => unknown(),
        Err(Error::JavaScript(error))
            if matches!(
                error.as_ref(),
                chunk_js::Error::Cancelled | chunk_js::Error::Deadline | chunk_js::Error::Heap
            ) =>
        {
            unknown()
        }
        Err(_) => wire::CommandFinished {
            state: wire::CommandCompletionState::Failed.into(),
            result_json: Vec::new(),
            error: "command failed; earlier effects may have completed".into(),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn owner_run(
    backend: Backend,
    id: ActionId,
    prepared: Prepared,
    mut incoming: tonic::Streaming<wire::CommandClientFrame>,
    output: Output,
    progress: watch::Sender<Progress>,
    cancel: CancellationToken,
    shutdown: CancellationToken,
) {
    let (effects, mut requests) = mpsc::channel::<PlatformEffect>(8);
    let call = prepared.call();
    let bytes = id.incarnation.len() + call.bytes() + scope_bytes(&prepared.scope) + prepared.input.len();
    let purpose = Purpose::Command(Arc::new(CommandBinding { scope: prepared.scope, input: prepared.input, effects }));
    let invocation = id.to_string();
    let acceptance =
        backend.submit_sized(bytes, |reply| Command::StartAction { id, call, purpose, retain: false, reply });
    let accepted = tokio::select! {
        ()=output.closed()=>Err(Error::Cancelled),
        ()=shutdown.cancelled()=>Err(Error::Cancelled),
        ()=cancel.cancelled()=>Err(Error::Cancelled),
        // Nothing has started yet, so any client frame, including Cancel, abandons acceptance outright.
        _=incoming.message()=>Err(Error::Cancelled),
        result=acceptance=>result,
        ()=tokio::time::sleep(Duration::from_secs(2))=>Err(Error::Cancelled),
    };
    let mut action = match accepted {
        Ok(action) => action,
        Err(error) => {
            let result = finished(Err(error));
            progress.send_replace(Progress { accepted: false, finished: Some(result.clone()) });
            send(&output, command_server_frame::Frame::Finished(result));
            return;
        }
    };
    progress.send_replace(Progress { accepted: true, finished: None });
    let mut pending = BTreeMap::<u32, PlatformEffect>::new();
    let result = if send(
        &output,
        command_server_frame::Frame::Accepted(wire::CommandAccepted {
            invocation_id: invocation.clone(),
            status_only: false,
        }),
    ) {
        loop {
            tokio::select! {
                ()=output.closed()=>break unknown(),
                ()=shutdown.cancelled()=>break unknown(),
                ()=cancel.cancelled()=>break unknown(),
                result=action.outcome()=>break finished(result),
                message=incoming.message()=>{
                    match message {
                        Ok(Some(wire::CommandClientFrame {frame:Some(command_client_frame::Frame::Reply(reply))}))=>{
                            let Some(effect)=pending.remove(&reply.sequence) else {break unknown();};
                            let result=effect_reply(&invocation,&effect,&reply);
                            effect.reply.finish(result);
                        }
                        // Cancel and any unexpected frame abort the run; the owner reports the outcome as unknown.
                        _=>break unknown(),
                    }
                }
                request=requests.recv()=>{
                    // The effect channel closes once the action finishes and its record goes.
                    let Some(effect)=request else {break finished(action.outcome().await);};
                    if effect.reply.cancellation.is_cancelled() {effect.reply.finish(Err(Error::Cancelled));continue;}
                    let sequence=effect.sequence;
                    let frame=wire::CommandEffect {sequence,operation_id:format!("action/{invocation}/platform/{sequence}"),request_json:effect.request.as_str().as_bytes().to_vec()};
                    if pending.len()>=8 || pending.contains_key(&sequence) || !send(&output,command_server_frame::Frame::Effect(frame)) {effect.reply.finish(Err(Error::Cancelled));break unknown();}
                    pending.insert(sequence,effect);
                }
            }
        }
    } else {
        unknown()
    };
    action.cancel();
    requests.close();
    for (_, effect) in pending {
        effect.reply.finish(Err(Error::Cancelled));
    }
    while let Ok(effect) = requests.try_recv() {
        effect.reply.finish(Err(Error::Cancelled));
    }
    progress.send_replace(Progress { accepted: true, finished: Some(result.clone()) });
    send(&output, command_server_frame::Frame::Finished(result));
}

fn effect_reply(invocation: &str, effect: &PlatformEffect, reply: &wire::CommandEffectReply) -> Result<Arc<str>> {
    if !reply.error.is_empty() {
        return Err(Error::Invalid("command effect failed; earlier effects may have completed"));
    }
    if reply.result_json.len() > 64 * 1024 {
        return Err(Error::Invalid("command effect result limit"));
    }
    let mut value = serde_json::from_slice(&reply.result_json)?;
    effect.result.normalize_api(&mut value);
    chunk_contract::validate_wire_value(&value).map_err(Error::Invalid)?;
    if !effect.result.accepts(&value)
        || (effect.receipt && value["operationId"] != format!("action/{invocation}/platform/{}", effect.sequence))
    {
        return Err(Error::Contract);
    }
    Ok(serde_json::to_string(&value)?.into())
}

async fn observe(
    invocation: String,
    mut incoming: tonic::Streaming<wire::CommandClientFrame>,
    output: Output,
    mut progress: watch::Receiver<Progress>,
    cancel: CancellationToken,
    shutdown: CancellationToken,
) {
    let mut announced = false;
    loop {
        let current = progress.borrow_and_update().clone();
        if current.accepted && !announced {
            if !send(
                &output,
                command_server_frame::Frame::Accepted(wire::CommandAccepted {
                    invocation_id: invocation.clone(),
                    status_only: true,
                }),
            ) {
                return;
            }
            announced = true;
        }
        if let Some(finished) = current.finished {
            send(&output, command_server_frame::Frame::Finished(finished));
            return;
        }
        tokio::select! {
            ()=output.closed()=>return,
            ()=shutdown.cancelled()=>return,
            changed=progress.changed()=>if changed.is_err() {send(&output,command_server_frame::Frame::Finished(unknown()));return;},
            message=incoming.message()=>{
                if matches!(message,Ok(Some(wire::CommandClientFrame {frame:Some(command_client_frame::Frame::Cancel(_))}))) {cancel.cancel();}
                return;
            }
        }
    }
}
