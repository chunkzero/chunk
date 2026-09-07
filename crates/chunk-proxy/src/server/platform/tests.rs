use std::sync::atomic::{AtomicUsize, Ordering};

use chunk_proto::v1::{
    BackendResult, BackendUpdate, BackendWatch,
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
    async fn call(&self, request: Request<BackendCall>) -> Result<Response<BackendResult>, Status> {
        assert_eq!(request.metadata().get("authorization").unwrap(), "Bearer test-token");
        let call = request.into_inner();
        assert_eq!(call.environment, "local");
        assert_eq!(call.deployment, "example");
        assert!(call.operation_id.is_empty());
        let caller: Value = serde_json::from_slice(&call.caller_json).unwrap();
        assert_eq!(caller["kind"], "proxy");
        assert!(caller["proxyId"].as_str().is_some_and(|value| !value.is_empty()));
        let result = match call.function.as_str() {
            "proxy/status" => {
                self.status.fetch_add(1, Ordering::SeqCst);
                if self.mode.load(Ordering::SeqCst) == 1 {
                    return Err(Status::unavailable("offline"));
                }
                json!({"motd": "Live backend", "online": 2, "max": 16})
            }
            "proxy/admit" => {
                if self.mode.load(Ordering::SeqCst) == 2 {
                    return Err(Status::deadline_exceeded("hook deadline"));
                }
                json!({"allow": self.mode.load(Ordering::SeqCst) == 0, "reason": "Closed"})
            }
            "proxy/route" => {
                self.routes.fetch_add(1, Ordering::SeqCst);
                json!({"key": "lobby", "session_type": "lobby", "machine_profile": "local"})
            }
            _ => panic!("unexpected hook"),
        };
        Ok(Response::new(BackendResult {
            revision: 1,
            result_json: serde_json::to_vec(&result).unwrap(),
        }))
    }

    type WatchStream = ReceiverStream<Result<BackendUpdate, Status>>;
    async fn watch(&self, _: Request<BackendWatch>) -> Result<Response<Self::WatchStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
}

#[tokio::test]
async fn status_is_live_and_failed_admission_never_routes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hooks = Hooks::default();
    let platform = Platform::new(PlatformTarget {
        backend: chunk_contract::BackendConnection {
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            token: "test-token".into(),
            environment: "local".into(),
            deployment: "example".into(),
        },
        control: chunk_contract::ControlConnection {
            endpoint: "http://127.0.0.1:1".into(),
            token: "unused".into(),
        },
    })
    .unwrap();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(BackendServer::new(hooks.clone()))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let good = platform.status("localhost").await.unwrap();
    assert!(String::from_utf8_lossy(&good).contains("Live backend"));
    assert_eq!(platform.route("uuid", "Alex").await.unwrap().key, "lobby");
    hooks.mode.store(1, Ordering::SeqCst);
    let bad = platform.status("localhost").await.unwrap();
    assert!(String::from_utf8_lossy(&bad).contains("temporarily unavailable"));
    let denied = platform.route("uuid", "Alex").await.unwrap_err();
    assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(denied.to_string(), "Closed");
    hooks.mode.store(2, Ordering::SeqCst);
    assert!(platform.route("uuid", "Alex").await.is_err());
    assert_eq!(hooks.status.load(Ordering::SeqCst), 2);
    assert_eq!(hooks.routes.load(Ordering::SeqCst), 1);
    server.abort();
    let _ = server.await;
}
