use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chunk_proto::v1::{
    ConfigurationRequest, ConfigurationResponse, PlayerDelivery, PlayerPreparation, PlayerSetup, ProcessIdentity,
    ProcessInventory, ProcessRegistration,
    gameplay_server::{Gameplay, GameplayServer},
    process_control_server::{ProcessControl, ProcessControlServer},
    supervisor_server::Supervisor,
};
use chunk_protocol::{
    BoundedArray, McString, RemainingBytes, Uuid, VarInt,
    versions::v26_1::{Handshake, LoginPluginRequest, LoginPluginResponse, LoginStart, LoginSuccess},
};
use prost::Message;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    time::timeout,
};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

use crate::{
    DeploymentRef, Launch, ManagedJvm, Phase, Status as RuntimeStatus,
    service::{Service, Shared},
    wire,
};

#[derive(Clone)]
struct FakeJvm {
    identity: ProcessIdentity,
    endpoint: String,
    unavailable: Arc<AtomicBool>,
}

#[tonic::async_trait]
impl ProcessControl for FakeJvm {
    async fn create_session(
        &self,
        _: Request<chunk_proto::v1::SessionCommand>,
    ) -> Result<Response<chunk_proto::v1::SessionInventory>, Status> {
        Err(Status::unimplemented("fixture"))
    }
    async fn finish_session(
        &self,
        _: Request<chunk_proto::v1::SessionCommand>,
    ) -> Result<Response<chunk_proto::v1::SessionInventory>, Status> {
        Err(Status::unimplemented("fixture"))
    }

    async fn inventory(&self, request: Request<ProcessIdentity>) -> Result<Response<ProcessInventory>, Status> {
        assert_eq!(request.get_ref(), &self.identity);
        assert_eq!(
            request.metadata().get("authorization").unwrap(),
            "Bearer child-credential"
        );
        if self.unavailable.load(Ordering::Acquire) {
            return Err(Status::unavailable("injected channel outage"));
        }
        Ok(Response::new(ProcessInventory {
            identity: Some(self.identity.clone()),
            tick_count: 1,
            ..Default::default()
        }))
    }
    async fn stop_process(&self, _: Request<ProcessIdentity>) -> Result<Response<ProcessIdentity>, Status> {
        Ok(Response::new(self.identity.clone()))
    }
}

#[tonic::async_trait]
impl Gameplay for FakeJvm {
    async fn withdraw_player(
        &self,
        _: Request<chunk_proto::v1::PlayerWithdrawal>,
    ) -> Result<Response<chunk_proto::v1::PlayerWithdrawal>, Status> {
        Err(Status::unimplemented("fixture"))
    }

    async fn configuration(&self, _: Request<ConfigurationRequest>) -> Result<Response<ConfigurationResponse>, Status> {
        unreachable!()
    }
    async fn prepare_player(&self, request: Request<PlayerDelivery>) -> Result<Response<PlayerPreparation>, Status> {
        assert_eq!(
            request.metadata().get("authorization").unwrap(),
            "Bearer child-credential"
        );
        Ok(Response::new(PlayerPreparation {
            operation_id: request.into_inner().operation_id,
            endpoint: self.endpoint.clone(),
            capability: vec![7; 32],
        }))
    }
}

fn request<T>(body: T, child: bool) -> Request<T> {
    let mut request = Request::new(body);
    request.metadata_mut().insert(
        "authorization",
        if child {
            "Bearer child-credential"
        } else {
            "Bearer runtime-credential"
        }
        .parse()
        .unwrap(),
    );
    request
}

async fn fixture(
    ingress: &TcpListener,
) -> (
    Arc<Shared>,
    FakeJvm,
    ProcessRegistration,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let identity = ProcessIdentity {
        deployment: Some(DeploymentRef {
            environment: "local".into(),
            deployment: "one".into(),
        }),
        runtime_id: "runtime".into(),
        process_id: "jvm".into(),
        generation: 1,
        machine_profile: "local".into(),
        artifact_digest: "test".into(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let player_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_endpoint = listener.local_addr().unwrap().to_string();
    let endpoint = player_listener.local_addr().unwrap().to_string();
    let fake = FakeJvm {
        identity: identity.clone(),
        endpoint: endpoint.clone(),
        unavailable: Arc::default(),
    };
    let stop = CancellationToken::new();
    let cancellation = stop.clone();
    let service = fake.clone();
    let task = tokio::spawn(async move {
        let cancel = cancellation.clone();
        let rpc = tonic::transport::Server::builder()
            .add_service(GameplayServer::new(service.clone()))
            .add_service(ProcessControlServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), cancel.cancelled_owned());
        let echo = async {
            let (mut stream, _) = player_listener.accept().await.unwrap();
            let _: Handshake = wire::read_packet(&mut stream).await.unwrap();
            let start: LoginStart = wire::read_packet(&mut stream).await.unwrap();
            wire::write_packet(
                &mut stream,
                &LoginPluginRequest {
                    message_id: VarInt(9),
                    channel: McString::new("chunk:delivery").unwrap(),
                    data: RemainingBytes::new(vec![]).unwrap(),
                },
            )
            .await
            .unwrap();
            let response: LoginPluginResponse = wire::read_packet(&mut stream).await.unwrap();
            assert_eq!(response.message_id, VarInt(9));
            let setup = PlayerSetup::decode(response.data.unwrap().as_slice()).unwrap();
            assert_eq!(setup.capability, vec![7; 32]);
            wire::write_packet(
                &mut stream,
                &LoginSuccess {
                    uuid: start.player_uuid,
                    username: start.username,
                    properties: BoundedArray::new(vec![]).unwrap(),
                },
            )
            .await
            .unwrap();
            let mut bytes = [0; 32];
            loop {
                let count = stream.read(&mut bytes).await.unwrap();
                if count == 0 {
                    break;
                }
                stream.write_all(&bytes[..count]).await.unwrap();
            }
            cancellation.cancelled().await;
        };
        tokio::select! { _ = rpc => {}, () = echo => {}, () = cancellation.cancelled() => {} }
    });
    let shared = Arc::new(Shared {
        identity: identity.clone(),
        child_credential: "child-credential".into(),
        credential: "runtime-credential".into(),
        ingress: ingress.local_addr().unwrap(),
        registration: Mutex::new(None),
        bindings: Mutex::new(BTreeMap::new()),
        shutdown: CancellationToken::new(),
        status: watch::channel(RuntimeStatus {
            phase: Phase::Ready,
            inventory: None,
            diagnostic: None,
        })
        .0,
    });
    let registration = ProcessRegistration {
        identity: Some(identity.clone()),
        control_endpoint,
        player_endpoint: endpoint,
        configuration: Some(ConfigurationResponse {
            deployment: identity.deployment.clone(),
            process_generation: 1,
            runtime_id: identity.runtime_id,
            protocol: 775,
        }),
    };
    (shared, fake, registration, stop, task)
}

async fn authenticate_registration(service: &Service, registration: &ProcessRegistration) {
    assert_eq!(
        service
            .register_process(Request::new(registration.clone()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    let mut invalid = registration.clone();
    invalid.identity.as_mut().unwrap().generation = 2;
    assert!(service.register_process(request(invalid, true)).await.is_err());
    service
        .register_process(request(registration.clone(), true))
        .await
        .unwrap();
    let mut changed = registration.clone();
    changed.player_endpoint = "127.0.0.1:9".into();
    assert!(service.register_process(request(changed, true)).await.is_err());
}

async fn rejected_setup(endpoint: &str, capability: Vec<u8>) {
    let mut stream = TcpStream::connect(endpoint).await.unwrap();
    client_setup(
        &mut stream,
        &PlayerSetup {
            operation_id: "one".into(),
            capability,
        },
    )
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_secs(1), stream.read_u8())
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn scoped_registration_and_single_use_relay_survive_lifecycle_reconciliation() {
    let ingress = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (shared, fake, registration, stop, task) = fixture(&ingress).await;
    let service = Service(shared.clone());
    authenticate_registration(&service, &registration).await;
    let delivery = PlayerDelivery {
        deployment: shared.identity.deployment.clone(),
        runtime_id: "runtime".into(),
        process_generation: 1,
        operation_id: "one".into(),
        ..Default::default()
    };
    let prepared = service
        .prepare_player(request(delivery.clone(), false))
        .await
        .unwrap()
        .into_inner();
    assert_ne!(prepared.capability, vec![7; 32]);
    assert_eq!(
        service
            .prepare_player(request(delivery.clone(), false))
            .await
            .unwrap()
            .into_inner(),
        prepared
    );
    assert!(
        service
            .prepare_player(request(
                PlayerDelivery {
                    owner_generation: 2,
                    ..delivery
                },
                false
            ))
            .await
            .is_err()
    );
    let relay = tokio::spawn(crate::relay::accept(ingress, shared.clone()));
    rejected_setup(&prepared.endpoint, vec![0; 32]).await;
    let mut stream = TcpStream::connect(&prepared.endpoint).await.unwrap();
    client_setup(
        &mut stream,
        &PlayerSetup {
            operation_id: "one".into(),
            capability: prepared.capability.clone(),
        },
    )
    .await
    .unwrap();
    let _: LoginSuccess = wire::read_packet(&mut stream).await.unwrap();
    for unavailable in [false, true, false] {
        fake.unavailable.store(unavailable, Ordering::Release);
        assert_eq!(shared.inventory().await.is_err(), unavailable);
        service
            .register_process(request(registration.clone(), true))
            .await
            .unwrap();
        stream.write_all(b"independent player bytes").await.unwrap();
        let mut bytes = [0; 24];
        timeout(Duration::from_secs(1), stream.read_exact(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&bytes, b"independent player bytes");
    }
    rejected_setup(&prepared.endpoint, prepared.capability).await;
    shared.shutdown.cancel();
    relay.await.unwrap().unwrap();
    stop.cancel();
    task.await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn startup_deadline_and_exited_child_leave_no_owned_process() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("child.log");
    let launch = |arguments| Launch {
        bootstrap_session: false,
        program: "/bin/sh".into(),
        arguments,
        deployment: DeploymentRef {
            environment: "local".into(),
            deployment: "one".into(),
        },
        machine_profile: "test".into(),
        artifact_digest: "test".into(),
        log_path: log.clone(),
        startup_timeout: Duration::from_millis(100),
    };
    assert!(
        ManagedJvm::launch(launch(vec!["-c".into(), "printf '%s' \"$$\"; exec sleep 60".into()]))
            .await
            .is_err()
    );
    let pid = std::fs::read_to_string(&log).unwrap();
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    assert!(
        ManagedJvm::launch(launch(vec!["-c".into(), "exit 7".into()]))
            .await
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn stalled_ticks_fail_health_and_stop_the_owned_child() {
    let ingress = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (shared, _, registration, stop, task) = fixture(&ingress).await;
    Service(shared.clone())
        .register_process(request(registration, true))
        .await
        .unwrap();
    let child = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    timeout(
        Duration::from_secs(15),
        crate::launch::monitor(shared.clone(), child, listener, ingress),
    )
    .await
    .unwrap();
    assert_eq!(shared.status.borrow().phase, Phase::Failed);
    assert_eq!(
        shared.status.borrow().diagnostic.as_deref(),
        Some("JVM ticks stopped advancing")
    );
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    stop.cancel();
    task.await.unwrap();
}

async fn client_setup(stream: &mut TcpStream, setup: &PlayerSetup) -> std::io::Result<()> {
    wire::write_packet(
        stream,
        &Handshake {
            protocol_version: VarInt(775),
            server_address: McString::new("localhost").unwrap(),
            server_port: 25565,
            next_state: VarInt(2),
        },
    )
    .await?;
    wire::write_packet(
        stream,
        &LoginStart {
            username: McString::new("Alex").unwrap(),
            player_uuid: Uuid([1; 16]),
        },
    )
    .await?;
    let challenge: LoginPluginRequest = wire::read_packet(stream).await?;
    wire::write_packet(
        stream,
        &LoginPluginResponse {
            message_id: challenge.message_id,
            data: Some(RemainingBytes::new(setup.encode_to_vec()).unwrap()),
        },
    )
    .await
}
