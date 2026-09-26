//! The client against a running management service, when `TEST_MANAGEMENT_URL` and `TEST_MANAGEMENT_EDGE_TOKEN`
//! name one and its `CHUNK_EDGE_TOKEN`; skipped otherwise.

use chunk_management::v1::{StartLoginRequest, WakeReason, WakeRequest, WatchRoutesRequest};
use chunk_management::{Client, Code};

fn service() -> Option<(String, String)> {
    Some((std::env::var("TEST_MANAGEMENT_URL").ok()?, std::env::var("TEST_MANAGEMENT_EDGE_TOKEN").ok()?))
}

#[tokio::test]
async fn talks_to_the_management_service() {
    let Some((url, edge_token)) = service() else { return };
    let login = Client::new(&url)
        .start_login(&StartLoginRequest { client_name: "chunk-management tests".into() })
        .await
        .expect("start login");
    assert!(!login.user_code.is_empty());

    let edge = Client::new(&url).with_token(edge_token);
    let mut routes = edge.watch_routes(&WatchRoutesRequest {}).await.expect("watch routes");
    assert!(routes.message().await.expect("first message").expect("a message").reset);

    let wake = WakeRequest {
        environment_id: "env_missing".into(),
        reason: WakeReason::Login.into(),
        client_address: "192.0.2.1".into(),
    };
    assert_eq!(edge.wake(&wake).await.unwrap_err().code(), Code::NotFound);
    assert_eq!(Client::new(&url).wake(&wake).await.unwrap_err().code(), Code::Unauthenticated);
}
