mod script;

use super::*;
use chunk_contract::{BackendConnection, CommandRoute, ControlConnection};
use chunk_proto::v1::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub(super) struct Service {
    pub movement: Arc<Mutex<Movement>>,
    pub assignment: Arc<Mutex<Assignment>>,
    pub commands: BTreeMap<String, Command>,
    pub allowed: Arc<AtomicBool>,
    pub catalog_unavailable: Arc<AtomicBool>,
    pub pending_method: Arc<AtomicBool>,
    pub cancels: Arc<AtomicUsize>,
    pub waiting: Arc<AtomicUsize>,
    pub prepared: Arc<AtomicUsize>,
    pub methods: Arc<Mutex<Vec<PrepareSessionMethodRequest>>>,
    pub starts: Arc<AtomicUsize>,
    pub polls: Arc<AtomicUsize>,
    pub reply_order: Arc<Mutex<Vec<u32>>>,
    pub replies: Arc<AtomicUsize>,
    pub release: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
pub(super) struct Movement {
    pub pending: Option<ClaimRequest>,
    pub error: Option<Status>,
    pub attempts: usize,
    pub reports: usize,
    pub lose_report: bool,
    pub stall_retries: bool,
    pub failure: Option<AbandonMoveRequest>,
}

#[tonic::async_trait]
impl backend_hooks_server::BackendHooks for Service {
    async fn manifest(&self, request: Request<()>) -> Result<Response<HookManifest>, Status> {
        auth(&request, "application")?;
        Ok(Response::new(HookManifest {
            deployment: "deployment".into(),
            manifest_json: serde_json::to_vec(&serde_json::json!({
                "version":1, "apps":{"lobby":""}, "scopes":{"":{"parent":null}}, "hooks":{}
            }))
            .unwrap(),
        }))
    }
    async fn invoke(&self, _: Request<InvokeHook>) -> Result<Response<HookResult>, Status> {
        Err(Status::unimplemented("unused"))
    }
}
fn auth<T>(request: &Request<T>, token: &str) -> Result<(), Status> {
    if request.metadata().get("authorization").is_none_or(|value| value != format!("Bearer {token}").as_str()) {
        return Err(Status::unauthenticated("wrong role"));
    }
    Ok(())
}
#[tonic::async_trait]
impl backend_commands_server::BackendCommands for Service {
    async fn catalog(&self, request: Request<CommandScope>) -> Result<Response<CommandCatalog>, Status> {
        auth(&request, "platform")?;
        assert_eq!(request.metadata().get("x-chunk-deployment").unwrap(), "deployment");
        if self.catalog_unavailable.load(Ordering::SeqCst) {
            return Err(Status::unavailable("commit pending"));
        }
        Ok(Response::new(CommandCatalog {
            commands_json: serde_json::to_vec(&self.commands).unwrap(),
            allowed_ids: if self.allowed.load(Ordering::SeqCst) {
                self.commands.keys().cloned().collect()
            } else {
                vec![]
            },
        }))
    }
    async fn suggest(
        &self,
        request: Request<CommandSuggestionRequest>,
    ) -> Result<Response<CommandSuggestionResult>, Status> {
        auth(&request, "platform")?;
        Ok(Response::new(CommandSuggestionResult { values: vec!["alpha".into(), "alpine".into(), "beta".into()] }))
    }
    async fn prepare(&self, request: Request<PrepareCommand>) -> Result<Response<PreparedCommand>, Status> {
        auth(&request, "platform")?;
        if !self.allowed.load(Ordering::SeqCst) {
            return Err(Status::permission_denied("denied"));
        }
        self.prepared.fetch_add(1, Ordering::SeqCst);
        let request = request.into_inner();
        Ok(Response::new(PreparedCommand {
            invocation_id: request.input.clone(),
            follow_player: request.input == "follow",
        }))
    }
    type RunStream = ReceiverStream<Result<CommandServerFrame, Status>>;
    async fn run(
        &self,
        request: Request<tonic::Streaming<CommandClientFrame>>,
    ) -> Result<Response<Self::RunStream>, Status> {
        auth(&request, "platform")?;
        let mut input = request.into_inner();
        let Some(command_client_frame::Frame::Start(start)) = input.message().await?.and_then(|frame| frame.frame)
        else {
            return Err(Status::invalid_argument("missing start"));
        };
        Ok(Response::new(script::start(self.clone(), start.invocation_id, input)))
    }
}
#[tonic::async_trait]
impl local_control_server::LocalControl for Service {
    async fn inspect(&self, request: Request<ClaimRequest>) -> Result<Response<Assignment>, Status> {
        auth(&request, "control")?;
        if self
            .assignment
            .lock()
            .unwrap()
            .claim
            .as_ref()
            .is_none_or(|claim| claim.operation_id != request.get_ref().operation_id)
        {
            return Err(Status::failed_precondition("stale"));
        }
        Ok(Response::new(self.assignment.lock().unwrap().clone()))
    }
    async fn prepare_session_method(
        &self,
        request: Request<PrepareSessionMethodRequest>,
    ) -> Result<Response<PreparedMethodHandle>, Status> {
        auth(&request, "control")?;
        self.methods.lock().unwrap().push(request.into_inner());
        Ok(Response::new(PreparedMethodHandle {
            operation_id: "captured-method".into(),
            deadline_ms: 9_999_999_999_999,
        }))
    }
    async fn start_prepared_method(
        &self,
        request: Request<PreparedMethodRequest>,
    ) -> Result<Response<SessionMethodResult>, Status> {
        auth(&request, "control")?;
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.pending_method.load(Ordering::SeqCst) {
            return Ok(Response::new(SessionMethodResult {
                operation_id: request.into_inner().operation_id,
                phase: i32::from(SessionMethodPhase::Accepted),
                result_json: String::new(),
                error: None,
            }));
        }
        Err(Status::unavailable("lost start response"))
    }
    async fn poll_prepared_method(
        &self,
        request: Request<PreparedMethodRequest>,
    ) -> Result<Response<SessionMethodResult>, Status> {
        auth(&request, "control")?;
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(SessionMethodResult {
            operation_id: request.into_inner().operation_id,
            phase: i32::from(if self.pending_method.load(Ordering::SeqCst) {
                SessionMethodPhase::Accepted
            } else {
                SessionMethodPhase::Completed
            }),
            result_json: "42".into(),
            error: None,
        }))
    }
    async fn cancel_prepared_method(
        &self,
        request: Request<PreparedMethodRequest>,
    ) -> Result<Response<SessionMethodResult>, Status> {
        auth(&request, "control")?;
        assert_eq!(request.get_ref().operation_id, "captured-method");
        self.cancels.fetch_add(1, Ordering::SeqCst);
        Err(Status::unimplemented("unused"))
    }
    async fn claim(&self, request: Request<ClaimRequest>) -> Result<Response<Assignment>, Status> {
        auth(&request, "control")?;
        let (error, stall) = {
            let mut movement = self.movement.lock().unwrap();
            assert_eq!(movement.pending.as_ref(), Some(request.get_ref()));
            movement.attempts += 1;
            (movement.error.clone(), movement.stall_retries && movement.attempts > 1)
        };
        if stall {
            tokio::time::sleep(super::super::WAIT_TIMEOUT).await;
        }
        Err(error.unwrap_or_else(|| Status::unimplemented("unused")))
    }
    async fn activate(&self, _: Request<ActivateClaim>) -> Result<Response<Assignment>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn cancel(&self, _: Request<ClaimRequest>) -> Result<Response<ClaimIdentity>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn reconcile_departure(&self, _: Request<ClaimRequest>) -> Result<Response<DepartureStatus>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn move_player(&self, _: Request<MovePlayerRequest>) -> Result<Response<ClaimRequest>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn poll_move(&self, request: Request<ClaimRequest>) -> Result<Response<PendingMove>, Status> {
        auth(&request, "control")?;
        Ok(Response::new(PendingMove { claim: self.movement.lock().unwrap().pending.clone() }))
    }
    async fn abandon_move(&self, request: Request<AbandonMoveRequest>) -> Result<Response<ClaimIdentity>, Status> {
        auth(&request, "control")?;
        let mut movement = self.movement.lock().unwrap();
        movement.reports += 1;
        if std::mem::take(&mut movement.lose_report) {
            return Err(Status::unavailable("lost failure report"));
        }
        let expected =
            movement.pending.as_ref().or_else(|| movement.failure.as_ref().and_then(|failure| failure.claim.as_ref()));
        assert_eq!(request.get_ref().claim.as_ref(), expected);
        movement.pending = None;
        movement.failure = Some(request.into_inner());
        Ok(Response::new(ClaimIdentity::default()))
    }
    async fn drain(&self, _: Request<DrainRequest>) -> Result<Response<DrainStatus>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn nodes(&self, _: Request<NodesRequest>) -> Result<Response<NodeList>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn players(&self, _: Request<PlayersRequest>) -> Result<Response<PlayerList>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn shutdown_node(&self, _: Request<ShutdownNodeRequest>) -> Result<Response<NodeStatus>, Status> {
        Err(Status::unimplemented("unused"))
    }
}

pub(super) struct Fixture {
    pub commands: Commands,
    pub service: Service,
    pub claim: ClaimRequest,
    pub assignment: Assignment,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    pub async fn new() -> Self {
        let commands = declarations();
        let claim = claim();
        let assignment = Assignment {
            claim: Some(ClaimIdentity {
                operation_id: "claim".into(),
                proxy_id: "proxy".into(),
                membership_generation: 1,
                delivery_generation: 1,
            }),
            phase: i32::from(ClaimPhase::Arrived),
            delivery: Some(PlayerDelivery {
                operation_id: "claim".into(),
                proxy_id: "proxy".into(),
                connection_id: "connection".into(),
                membership_generation: 1,
                owner_generation: 1,
                player: Some(PlayerRef { id: "player".into() }),
                session: Some(SessionRef { id: "session".into() }),
                deployment: Some(DeploymentRef { environment: "environment".into(), deployment: "deployment".into() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let service = Service {
            movement: Arc::default(),
            assignment: Arc::new(Mutex::new(assignment.clone())),
            commands,
            allowed: Arc::new(AtomicBool::new(true)),
            catalog_unavailable: Arc::default(),
            pending_method: Arc::default(),
            cancels: Arc::default(),
            waiting: Arc::default(),
            prepared: Arc::default(),
            methods: Arc::default(),
            starts: Arc::default(),
            polls: Arc::default(),
            reply_order: Arc::default(),
            replies: Arc::default(),
            release: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let server_service = service.clone();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(backend_hooks_server::BackendHooksServer::new(server_service.clone()))
                .add_service(backend_commands_server::BackendCommandsServer::new(server_service.clone()))
                .add_service(local_control_server::LocalControlServer::new(server_service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stopped.cancelled())
                .await
                .unwrap();
        });
        let platform = Platform::new(crate::PlatformTarget {
            backend: BackendConnection {
                endpoint: endpoint.clone(),
                token: "application".into(),
                platform_token: Some("platform".into()),
                environment: "environment".into(),
                deployment: "deployment".into(),
            },
            control: ControlConnection { endpoint, token: "control".into() },
        })
        .unwrap();
        let manifest = Arc::new(serde_json::from_value::<DomainManifest>(serde_json::json!({"version":1,"apps":{"lobby":""},"scopes":{"":{"parent":null}},"hooks":{},"commands":service.commands})).unwrap());
        let (output, receiver) = mpsc::channel(32);
        let (state, current) = watch::channel(None);
        let mut commands = Commands {
            tasks: Tasks {
                platform,
                connection: CancellationToken::new(),
                invocation: CancellationToken::new(),
                current,
                output,
                capacity: Arc::new(Semaphore::new(8)),
                methods: Arc::new(Semaphore::new(8)),
                effects: Arc::new(Semaphore::new(8)),
            },
            state,
            output: receiver,
            manifest: Some(manifest),
            origin: None,
            catalog: None,
            descriptors: BTreeMap::new(),
            allowed: BTreeSet::new(),
            tree_received: false,
            refreshing: false,
        };
        commands.bind(&claim, &assignment).unwrap();
        Self { commands, service, claim, assignment, stop, server }
    }
    pub async fn close(self) {
        let platform = self.commands.tasks.platform.clone();
        drop(self.commands);
        platform.cleanup.close();
        tokio::time::timeout(std::time::Duration::from_secs(3), platform.cleanup.wait()).await.unwrap();
        self.stop.cancel();
        self.server.await.unwrap();
    }
}

fn declarations() -> BTreeMap<String, Command> {
    let mut commands: BTreeMap<String, Command> = ["echo", "slow", "follow"]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                Command {
                    domain: String::new(),
                    name: name.into(),
                    aliases: vec![],
                    export: name.into(),
                    permission: None,
                    follow_player: name == "follow",
                    routes: vec![CommandRoute { literals: vec![], arguments: vec![] }],
                },
            )
        })
        .collect();
    commands.insert(
        "travel".into(),
        Command {
            domain: String::new(),
            name: "travel".into(),
            aliases: vec![],
            export: "travel".into(),
            permission: None,
            follow_player: false,
            routes: vec![CommandRoute {
                literals: vec![],
                arguments: vec![chunk_contract::CommandArgument {
                    name: "place".into(),
                    parser: chunk_contract::CommandParser::Word,
                    min: None,
                    max: None,
                    suggestions: Some(chunk_contract::CommandSuggestions::Query(chunk_contract::SuggestionQuery {
                        query: "shared/places/suggest".into(),
                    })),
                }],
            }],
        },
    );
    commands
}

fn claim() -> ClaimRequest {
    ClaimRequest {
        operation_id: "claim".into(),
        proxy_id: "proxy".into(),
        connection_id: "connection".into(),
        identity: Some(Identity { uuid: "player".into(), username: "Player".into(), ..Default::default() }),
        demand: Some(SessionDemand {
            key: "lobby".into(),
            session_type: "lobby/default".into(),
            machine_profile: "local".into(),
        }),
        source: None,
    }
}
