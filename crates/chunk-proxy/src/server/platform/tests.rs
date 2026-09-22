use std::sync::atomic::{AtomicUsize, Ordering};

use chunk_proto::v1::{
    BackendMutation, BackendResult, BackendUpdate, BackendWatchGroup,
    backend_server::{Backend, BackendServer},
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Response, Status};

use super::*;

#[derive(Clone, Default)]
struct Hooks {
    mode: Arc<AtomicUsize>,
    status: Arc<AtomicUsize>,
    routes: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl Backend for Hooks {
    async fn query(&self, request: Request<BackendQuery>) -> Result<Response<BackendResult>, Status> {
        assert_eq!(request.metadata().get("authorization").unwrap(), "Bearer test-token");
        assert_eq!(request.metadata().get("x-chunk-environment").unwrap(), "local");
        assert_eq!(request.metadata().get("x-chunk-deployment").unwrap(), "example");
        let call = request.into_inner();
        let caller: Value = serde_json::from_slice(&call.caller_json).unwrap();
        assert_eq!(caller["kind"], "proxy");
        assert!(caller["proxyId"].as_str().is_some_and(|value| !value.is_empty()));
        let result = match call.function.as_str() {
            "shared/proxy/status" => {
                self.status.fetch_add(1, Ordering::SeqCst);
                if self.mode.load(Ordering::SeqCst) == 1 {
                    return Err(Status::unavailable("offline"));
                }
                json!({"motd": "Live backend", "online": 2, "max": 16})
            }
            "shared/proxy/admit" => {
                if self.mode.load(Ordering::SeqCst) == 2 {
                    return Err(Status::deadline_exceeded("hook deadline"));
                }
                json!({"allow": self.mode.load(Ordering::SeqCst) == 0, "reason": "Closed"})
            }
            "shared/proxy/route" => {
                self.routes.fetch_add(1, Ordering::SeqCst);
                json!({"key": "lobby", "session_type": "lobby", "machine_profile": "local"})
            }
            _ => panic!("unexpected hook"),
        };
        Ok(Response::new(BackendResult { revision: 1, result_json: serde_json::to_vec(&result).unwrap() }))
    }

    async fn check_deployment(&self, _: Request<()>) -> Result<Response<()>, Status> {
        Err(Status::unimplemented("unused"))
    }

    async fn mutate(&self, _: Request<BackendMutation>) -> Result<Response<BackendResult>, Status> {
        Err(Status::unimplemented("unused"))
    }

    type WatchGroupStream = ReceiverStream<Result<BackendUpdate, Status>>;
    async fn watch_group(&self, _: Request<BackendWatchGroup>) -> Result<Response<Self::WatchGroupStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
}

#[tokio::test]
async fn status_is_live_and_failed_admission_never_routes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hooks = Hooks::default();
    let platform = Platform::new(PlatformTarget {
        backend: chunk_contract::BackendConnection {
            platform_token: None,
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            token: "test-token".into(),
            environment: "local".into(),
            deployment: "example".into(),
        },
        control: chunk_contract::ControlConnection { endpoint: "http://127.0.0.1:1".into(), token: "unused".into() },
    })
    .unwrap();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(BackendServer::new(hooks.clone()))
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

async fn route(platform: &Platform) -> io::Result<SessionDemand> {
    platform
        .route_claim(&chunk_proto::v1::ClaimRequest {
            operation_id: uuid::Uuid::new_v4().to_string(),
            proxy_id: platform.proxy_id.clone(),
            connection_id: "test".into(),
            identity: Some(chunk_proto::v1::Identity {
                uuid: "uuid".into(),
                username: "Alex".into(),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
}
