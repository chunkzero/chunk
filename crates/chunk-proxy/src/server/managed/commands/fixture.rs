mod core_service;
mod script;

pub(super) use core_service::Held;

use super::*;
use crate::server::platform::generation;
use chunk_contract::{BackendConnection, CommandRoute};
use chunk_proto::{
    sync::v1::{self as sync, ClaimPhase, GatewayLogin, GatewayMove, core_server},
    v1::*,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub(super) struct Service {
    pub movement: Arc<Mutex<Movement>>,
    /// The one claim the gateway's topic holds.
    pub claim: Arc<Mutex<Held>>,
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
    /// Activations answered as waiting for the rest of a roster before one succeeds.
    pub roster_waits: Arc<AtomicUsize>,
    pub activations: Arc<AtomicUsize>,
    /// Counts claim-state changes; open streams send a new snapshot for each.
    pub published: Arc<watch::Sender<u64>>,
    /// Ends open streams at their next publication and refuses new ones.
    pub watch_down: Arc<AtomicBool>,
    pub refused_watches: Arc<AtomicUsize>,
    pub logins: Arc<Mutex<Logins>>,
    /// The next call naming the open stream drops it, then is stopped once the gateway resubscribes, before the new
    /// stream's first update.
    pub supersede: Arc<AtomicBool>,
    /// Steps a supersede: first drops the open stream, then releases the next one's first update.
    superseded: Arc<tokio::sync::Notify>,
    /// The newest stream; calls naming another are stopped.
    stream: Arc<Mutex<String>>,
    streams: Arc<AtomicUsize>,
    watches: CancellationToken,
}

#[derive(Default)]
pub(super) struct Logins {
    /// Each login claimed, with its operation ID, in order.
    pub claims: Vec<(String, GatewayLogin)>,
    /// The operation ID of each claim withdrawn, in order.
    pub cancels: Vec<String>,
    /// Logins routed with this deployment are rejected for routing again.
    pub retired: Option<String>,
    /// The next routing waits for this, then fails.
    pub unroutable: Option<tokio::sync::oneshot::Receiver<()>>,
}

#[derive(Default)]
pub(super) struct Movement {
    pub pending: Option<GatewayMove>,
    pub error: Option<sync::Error>,
    pub attempts: usize,
    pub reports: usize,
    pub lose_report: bool,
    pub stall_retries: bool,
    /// The operation ID and reason of the move abandoned.
    pub failure: Option<(String, String)>,
}

#[tonic::async_trait]
impl backend_hooks_server::BackendHooks for Service {
    async fn manifest(&self, request: Request<()>) -> Result<Response<HookManifest>, Status> {
        auth(&request, "application")?;
        Ok(Response::new(HookManifest {
            deployment: request.metadata().get("x-chunk-deployment").unwrap().to_str().unwrap().into(),
            manifest_json: serde_json::to_vec(&serde_json::json!({
                "version":1, "apps":{"lobby":""}, "scopes":{"":{"parent":null}},
                "hooks":{"shared/domains/hooks/route":{"domain":"","event":"player.route","export":"route"}}
            }))
            .unwrap(),
        }))
    }
    async fn invoke(&self, request: Request<InvokeHook>) -> Result<Response<HookResult>, Status> {
        auth(&request, "platform")?;
        let unroutable = self.logins.lock().unwrap().unroutable.take();
        if let Some(unroutable) = unroutable {
            let _ = unroutable.await;
            return Err(Status::unavailable("routing failed"));
        }
        let route = serde_json::json!({"key":"lobby","session_type":"lobby/default","machine_profile":"local"});
        Ok(Response::new(HookResult { result_json: serde_json::to_vec(&route).unwrap() }))
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
    type WatchStream = ReceiverStream<Result<ClaimUpdate, Status>>;
    async fn watch(&self, _: Request<WatchRequest>) -> Result<Response<Self::WatchStream>, Status> {
        Err(Status::unimplemented("unused"))
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
    async fn claim(&self, _: Request<ClaimRequest>) -> Result<Response<Assignment>, Status> {
        Err(Status::unimplemented("unused"))
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
    async fn abandon_move(&self, _: Request<AbandonMoveRequest>) -> Result<Response<ClaimIdentity>, Status> {
        Err(Status::unimplemented("unused"))
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
    pub identity: ClaimIdentity,
    pub session: String,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    pub async fn new() -> Self {
        let commands = declarations();
        let claim = claim();
        let held = Held {
            operation: "claim".into(),
            generation: sync::Position { epoch: 1, revision: 1 },
            phase: ClaimPhase::Arrived,
        };
        let identity = ClaimIdentity {
            operation_id: "claim".into(),
            proxy_id: "proxy".into(),
            membership_generation: generation(&held.generation),
            delivery_generation: generation(&held.generation),
        };
        let service = Service {
            movement: Arc::default(),
            claim: Arc::new(Mutex::new(held)),
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
            roster_waits: Arc::default(),
            activations: Arc::default(),
            published: Arc::new(watch::Sender::new(1)),
            watch_down: Arc::default(),
            refused_watches: Arc::default(),
            logins: Arc::default(),
            supersede: Arc::default(),
            superseded: Arc::default(),
            stream: Arc::default(),
            streams: Arc::default(),
            watches: CancellationToken::new(),
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
                .add_service(core_server::CoreServer::new(server_service.clone()))
                .add_service(local_control_server::LocalControlServer::new(server_service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stopped.cancelled())
                .await
                .unwrap();
        });
        let backend = BackendConnection {
            endpoint: endpoint.clone(),
            token: "application".into(),
            platform_token: Some("platform".into()),
            environment: "environment".into(),
            deployment: "deployment".into(),
        };
        let gateway = crate::GatewayCredential { id: "proxy".into(), credential: "gateway".into() };
        let target = crate::PlatformTarget { core: endpoint, gateway, backend, control_token: "control".into() };
        let platform = Platform::new(target).unwrap();
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
        let session = String::from("session");
        commands.bind(&claim, &identity, &session).unwrap();
        Self { commands, service, claim, identity, session, stop, server }
    }
    /// Publishes the current claim state and waits until the platform's view reflects it.
    pub async fn sync(&self) {
        let position = self.service.publish();
        let synced = self.commands.tasks.platform.claims(|view| view.passed(Some(&position)).then_some(()));
        tokio::time::timeout(std::time::Duration::from_secs(3), synced).await.unwrap().unwrap();
    }
    pub async fn close(self) {
        let platform = self.commands.tasks.platform.clone();
        drop(self.commands);
        platform.cleanup.close();
        tokio::time::timeout(std::time::Duration::from_secs(3), platform.cleanup.wait()).await.unwrap();
        self.service.watches.cancel();
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
        deployment: String::new(),
    }
}
