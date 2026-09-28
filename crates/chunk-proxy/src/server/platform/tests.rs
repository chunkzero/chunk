use std::sync::atomic::{AtomicUsize, Ordering};

use chunk_proto::sync::v1::{
    self as sync, CallResponse, ManifestResult, SubscribeRequest, Update, call_response,
    core_server::{Core, CoreServer},
    error::Code,
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::Response;

use super::*;

/// A fake core whose deployment declares no domain manifest, so its legacy `shared/proxy/*` queries answer.
#[derive(Clone, Default)]
struct Hooks {
    mode: Arc<AtomicUsize>,
    status: Arc<AtomicUsize>,
    routes: Arc<AtomicUsize>,
}

impl Hooks {
    fn run(&self, call: &CallRequest) -> Result<Vec<u8>, sync::Error> {
        assert_eq!(call.deployment, "example");
        assert!(call.stream.is_empty() && call.caller.is_none());
        if call.method == "chunk:manifest" {
            return Ok(ManifestResult { deployment: call.deployment.clone(), manifest_json: vec![] }.encode_to_vec());
        }
        assert!(call.operation_id.is_empty());
        let failed = |message: &str| Err(sync::Error { code: Code::Unavailable.into(), message: message.into() });
        let result = match call.method.as_str() {
            "shared/proxy/status" => {
                self.status.fetch_add(1, Ordering::SeqCst);
                if self.mode.load(Ordering::SeqCst) == 1 {
                    return failed("offline");
                }
                json!({"motd": "Live backend", "online": 2, "max": 16})
            }
            "shared/proxy/admit" => {
                if self.mode.load(Ordering::SeqCst) == 2 {
                    return failed("hook deadline");
                }
                json!({"allow": self.mode.load(Ordering::SeqCst) == 0, "reason": "Closed"})
            }
            "shared/proxy/route" => {
                self.routes.fetch_add(1, Ordering::SeqCst);
                json!({"key": "lobby", "session_type": "lobby", "machine_profile": "local"})
            }
            _ => panic!("unexpected hook"),
        };
        Ok(serde_json::to_vec(&result).unwrap())
    }
}

#[tonic::async_trait]
impl Core for Hooks {
    async fn call(&self, request: Request<CallRequest>) -> Result<Response<CallResponse>, tonic::Status> {
        assert_eq!(request.metadata().get("authorization").unwrap(), "Bearer gateway");
        let outcome = match self.run(request.get_ref()) {
            Ok(result) => call_response::Outcome::Result(result),
            Err(error) => call_response::Outcome::Error(error),
        };
        Ok(Response::new(CallResponse { position: None, outcome: Some(outcome) }))
    }

    type SubscribeStream = ReceiverStream<Result<Update, tonic::Status>>;
    async fn subscribe(&self, _: Request<SubscribeRequest>) -> Result<Response<Self::SubscribeStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused"))
    }
}

#[tokio::test]
async fn status_is_live_and_failed_admission_never_routes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hooks = Hooks::default();
    let platform = Platform::new(PlatformTarget {
        core: format!("http://{}", listener.local_addr().unwrap()),
        gateway: crate::GatewayCredential { id: "proxy".into(), credential: "gateway".into() },
        deployment: "example".into(),
    })
    .unwrap();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(CoreServer::new(hooks.clone()))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let good = platform.status("localhost").await.unwrap();
    assert!(String::from_utf8_lossy(&good).contains("Live backend"));
    assert_eq!(route(&platform).await.unwrap().key, "lobby");
    let status_permits = platform.status_hooks.acquire_many(64).await.unwrap();
    let saturated = platform.status("localhost").await.unwrap();
    assert!(String::from_utf8_lossy(&saturated).contains("temporarily unavailable"));
    assert_eq!(route(&platform).await.unwrap().key, "lobby");
    drop(status_permits);
    hooks.mode.store(1, Ordering::SeqCst);
    let bad = platform.status("localhost").await.unwrap();
    assert!(String::from_utf8_lossy(&bad).contains("temporarily unavailable"));
    let denied = route(&platform).await.unwrap_err();
    assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(denied.to_string(), "Closed");
    hooks.mode.store(2, Ordering::SeqCst);
    assert!(route(&platform).await.is_err());
    assert_eq!(hooks.status.load(Ordering::SeqCst), 2);
    assert_eq!(hooks.routes.load(Ordering::SeqCst), 2);
    server.abort();
    let _ = server.await;
}

#[test]
fn core_endpoint_must_be_private() {
    for core in ["http://127.0.0.1:7070", "http://10.0.0.2:7070", "http://[fdaa::1]:7070"] {
        assert!(endpoint(core).is_ok(), "{core}");
    }
    for core in
        ["http://203.0.113.1:7070", "http://[2001:db8::1]:7070", "http://169.254.169.254:80", "https://10.0.0.2:7070"]
    {
        assert!(endpoint(core).is_err(), "{core}");
    }
}

async fn route(platform: &Platform) -> io::Result<SessionDemand> {
    platform
        .route_claim(&Claim {
            operation_id: uuid::Uuid::new_v4().to_string(),
            connection_id: "test".into(),
            player: chunk_proto::sync::v1::PlayerIdentity {
                uuid: "uuid".into(),
                username: "Alex".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
}
