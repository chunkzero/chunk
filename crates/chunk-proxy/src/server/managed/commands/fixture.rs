mod core_service;
mod script;

pub(super) use core_service::Held;

use super::*;
use crate::server::platform::generation;
use chunk_contract::CommandRoute;
use chunk_proto::sync::v1::{self as sync, ClaimPhase, GatewayLogin, GatewayMove, core_server};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize},
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Status};

#[derive(Clone)]
pub(super) struct Service {
    pub movement: Arc<Mutex<Movement>>,
    /// The one claim the gateway's topic holds.
    pub claim: Arc<Mutex<Held>>,
    pub commands: BTreeMap<String, Command>,
    pub allowed: Arc<AtomicBool>,
    pub catalog_unavailable: Arc<AtomicBool>,
    /// Commands started, by operation ID.
    pub runs: Arc<Mutex<BTreeMap<String, script::Run>>>,
    /// Subscriptions opened to command topics.
    pub follows: Arc<AtomicUsize>,
    pub waiting: Arc<AtomicUsize>,
    pub replies: Arc<AtomicUsize>,
    pub release: Arc<tokio::sync::Notify>,
    /// Commands finish at the next `release` once their effect is pending, without waiting for its acknowledgment.
    pub outstanding: Arc<AtomicBool>,
    /// Starts wait for `admit` before they're admitted.
    pub admission: Arc<AtomicBool>,
    pub admit: Arc<tokio::sync::Notify>,
    /// Starts that waited for admission.
    pub queued: Arc<AtomicUsize>,
    /// A start's response waits until a command topic is subscribed again, and that subscription opens only shortly after
    /// the response.
    pub reopening: Arc<AtomicBool>,
    reopened: Arc<tokio::sync::Notify>,
    reopen: Arc<tokio::sync::Notify>,
    /// Activations answered as waiting for the rest of a roster before one succeeds.
    pub roster_waits: Arc<AtomicUsize>,
    pub activations: Arc<AtomicUsize>,
    /// Counts claim-state changes; open streams send a new snapshot for each.
    pub published: Arc<watch::Sender<u64>>,
    /// Ends open streams at their next publication and refuses new ones.
    pub watch_down: Arc<AtomicBool>,
    pub refused_watches: Arc<AtomicUsize>,
    pub logins: Arc<Mutex<Logins>>,
    pub placement: Arc<Mutex<Placement>>,
    /// The next call naming the open stream drops it, then is stopped once the gateway resubscribes, before the new
    /// stream's first update.
    pub supersede: Arc<AtomicBool>,
    /// Steps a supersede: first drops the open stream, then releases the next one's first update.
    superseded: Arc<tokio::sync::Notify>,
    /// The newest stream; calls naming another are stopped.
    stream: Arc<Mutex<String>>,
    streams: Arc<AtomicUsize>,
    prepared: Arc<AtomicUsize>,
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

/// The deployments the fake core knows, and how it places claims among them.
#[derive(Default)]
pub(super) struct Placement {
    /// Domain manifests by deployment; others get the fixture's own. Routing sends a login to its first app.
    pub manifests: BTreeMap<String, serde_json::Value>,
    /// The deployments whose admission hooks deny.
    pub denying: BTreeSet<String>,
    /// The hooks run, as deployment, hook and payload.
    pub hooks: Vec<(String, String, serde_json::Value)>,
    /// Claims admitted in another deployment are refused. Unset, every one is placed.
    pub current: Option<String>,
    /// Where a login's player returns unless it declines: the session's deployment and destination.
    pub returns: Option<(String, sync::SessionDemand)>,
    /// The deployment and destination a move's operation already holds a reservation in.
    pub reserved: Option<(String, sync::SessionDemand)>,
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

fn auth<T>(request: &Request<T>, token: &str) -> Result<(), Status> {
    if request.metadata().get("authorization").is_none_or(|value| value != format!("Bearer {token}").as_str()) {
        return Err(Status::unauthenticated("wrong role"));
    }
    Ok(())
}

pub(super) struct Fixture {
    pub commands: Commands,
    pub service: Service,
    pub claim: Claim,
    pub identity: ClaimIdentity,
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
        let identity =
            ClaimIdentity { operation_id: "claim".into(), delivery_generation: generation(&held.generation) };
        let service = Service {
            movement: Arc::default(),
            claim: Arc::new(Mutex::new(held)),
            commands,
            allowed: Arc::new(AtomicBool::new(true)),
            catalog_unavailable: Arc::default(),
            runs: Arc::default(),
            follows: Arc::default(),
            waiting: Arc::default(),
            replies: Arc::default(),
            release: Arc::default(),
            outstanding: Arc::default(),
            admission: Arc::default(),
            admit: Arc::default(),
            queued: Arc::default(),
            reopening: Arc::default(),
            reopened: Arc::default(),
            reopen: Arc::default(),
            roster_waits: Arc::default(),
            activations: Arc::default(),
            published: Arc::new(watch::Sender::new(1)),
            watch_down: Arc::default(),
            refused_watches: Arc::default(),
            logins: Arc::default(),
            placement: Arc::default(),
            supersede: Arc::default(),
            superseded: Arc::default(),
            stream: Arc::default(),
            streams: Arc::default(),
            prepared: Arc::default(),
            watches: CancellationToken::new(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let server_service = service.clone();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(core_server::CoreServer::new(server_service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stopped.cancelled())
                .await
                .unwrap();
        });
        let gateway = crate::GatewayCredential { id: "proxy".into(), credential: "gateway".into() };
        let target = crate::PlatformTarget { core: endpoint, gateway, deployment: "deployment".into() };
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
        commands.bind(&claim, &identity).unwrap();
        Self { commands, service, claim, identity, stop, server }
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

fn claim() -> Claim {
    Claim {
        operation_id: "claim".into(),
        connection_id: "connection".into(),
        player: sync::PlayerIdentity { uuid: "player".into(), username: "Player".into(), ..Default::default() },
        demand: sync::SessionDemand {
            key: "lobby".into(),
            session_type: "lobby/default".into(),
            machine_profile: "local".into(),
        },
        source: None,
        deployment: String::new(),
        decline_reconnect: false,
    }
}
