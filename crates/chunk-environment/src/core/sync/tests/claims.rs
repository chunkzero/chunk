//! A gateway's claim lifecycle over `chunk:*` calls.

use super::*;
use chunk_proto::sync::v1::{
    ActivateResult, ClaimArguments, ClaimPhase, ClaimResult, GatewayClaim, GatewayLogin, PlayerIdentity, SessionDemand,
    WithdrawResult, claim_result,
};

impl Fixture {
    /// Follows `gateway/<id>` as `credential`, returning the stream and its first update.
    async fn follow(&mut self, credential: &str, id: &str) -> (Streaming<Update>, Update) {
        let subscription = SubscribeRequest { topic: format!("gateway/{id}"), ..SubscribeRequest::default() };
        let mut updates = self.client.subscribe(authorized(subscription, credential)).await.unwrap().into_inner();
        let first = next(&mut updates).await;
        (updates, first)
    }

    async fn platform(
        &mut self,
        credential: &str,
        stream: &str,
        operation: &str,
        method: &str,
        arguments: &impl Message,
    ) -> CallResponse {
        let message = CallRequest {
            operation_id: operation.into(),
            method: method.into(),
            arguments: arguments.encode_to_vec(),
            stream: stream.into(),
            ..CallRequest::default()
        };
        self.client.call(authorized(message, credential)).await.unwrap().into_inner()
    }
}

fn login(connection: &str) -> ClaimArguments {
    ClaimArguments {
        login: Some(GatewayLogin {
            connection_id: connection.into(),
            player: Some(PlayerIdentity {
                uuid: runtime::PLAYER.into(),
                username: "player".into(),
                properties: vec![],
            }),
            demand: Some(SessionDemand {
                key: "lobby".into(),
                session_type: "bridge/default".into(),
                machine_profile: "small".into(),
            }),
            deployment: String::new(),
        }),
    }
}

fn result<T: Message + Default>(response: &CallResponse) -> T {
    match &response.outcome {
        Some(Outcome::Result(result)) => T::decode(result.as_slice()).unwrap(),
        outcome => panic!("expected a result, got {outcome:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_claims_activates_and_sees_its_player_arrive() {
    let (jvm, server) = runtime::Runtime::start();
    let mut fixture = Fixture::with_host(Arc::new(jvm)).await;
    fixture.control.activate_release(runtime::release()).unwrap();
    let gateway = fixture.gateway.clone();
    let (mut updates, first) = fixture.follow(&gateway, "proxy").await;

    let claimed = fixture.platform(&gateway, &first.stream, "login", "chunk:claim", &login("connection")).await;
    let Some(claim_result::Outcome::Assignment(assignment)) = result::<ClaimResult>(&claimed).outcome else {
        panic!("expected an assignment");
    };
    assert_eq!(assignment.capability.len(), 32);
    let activated = fixture.platform(&gateway, &first.stream, "login", "chunk:activate", &()).await;
    assert!(!result::<ActivateResult>(&activated).waiting);

    loop {
        let update = next(&mut updates).await;
        let arrived = update.upserts.iter().any(|entry| match &entry.state {
            Some(State::Value(value)) if entry.key == "login" => {
                let claim = GatewayClaim::decode(value.as_slice()).unwrap();
                claim.phase() == ClaimPhase::Arrived && claim.generation == assignment.generation
            }
            _ => false,
        });
        if arrived {
            assert!(revision(update.position.as_ref()) >= revision(activated.position.as_ref()));
            break;
        }
    }
    drop(updates);
    fixture.stop().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_calls_naming_a_superseded_or_foreign_stream_are_stopped() {
    let mut fixture = Fixture::start().await;
    let gateway = fixture.gateway.clone();
    let other = fixture.gateways.mint("other");
    let (older, superseded) = fixture.follow(&gateway, "proxy").await;
    let (newer, current) = fixture.follow(&gateway, "proxy").await;

    let stale = [(&gateway, superseded.stream.as_str()), (&gateway, ""), (&other, current.stream.as_str())];
    for (credential, stream) in stale {
        let response = fixture.platform(credential, stream, "login", "chunk:withdraw", &()).await;
        assert_eq!(code(&response), Code::Stopped);
    }
    let withdrawn = fixture.platform(&gateway, &current.stream, "login", "chunk:withdraw", &()).await;
    assert!(result::<WithdrawResult>(&withdrawn).unknown);
    drop((older, newer));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_replays_its_outcome_and_rejects_a_changed_request_or_another_gateway() {
    let (jvm, server) = runtime::Runtime::start();
    let mut fixture = Fixture::with_host(Arc::new(jvm)).await;
    fixture.control.activate_release(runtime::release()).unwrap();
    let gateway = fixture.gateway.clone();
    let (updates, first) = fixture.follow(&gateway, "proxy").await;

    let claimed = fixture.platform(&gateway, &first.stream, "login", "chunk:claim", &login("connection")).await;
    let replayed = fixture.platform(&gateway, &first.stream, "login", "chunk:claim", &login("connection")).await;
    assert_eq!(result::<ClaimResult>(&replayed), result::<ClaimResult>(&claimed));
    let changed = fixture.platform(&gateway, &first.stream, "login", "chunk:claim", &login("elsewhere")).await;
    assert_eq!(code(&changed), Code::OperationMismatch);

    let other = fixture.gateways.mint("other");
    let (foreign, stream) = fixture.follow(&other, "other").await;
    let withdrawn = fixture.platform(&other, &stream.stream, "login", "chunk:withdraw", &()).await;
    assert_eq!(code(&withdrawn), Code::Denied);
    drop((updates, foreign));
    fixture.stop().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_gateways_call_claim_methods() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    for credential in [cli.as_str(), JVM] {
        let response = fixture.platform(credential, "", "login", "chunk:claim", &login("connection")).await;
        assert_eq!(code(&response), Code::Denied);
    }
    let unknown = fixture.platform(&cli, "", "login", "chunk:unknown", &()).await;
    assert_eq!(code(&unknown), Code::Invalid);
    fixture.stop().await;
}
