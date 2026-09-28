//! A gateway's claim lifecycle over `chunk:*` calls.

use super::{runtime::with_jvm, *};
use chunk_control::MoveRequest;
use chunk_proto::sync::v1::{
    AbandonMoveArguments, ActivateResult, ClaimArguments, ClaimPhase, ClaimResult, DepartResult, GatewayClaim,
    GatewayLogin, PlayerIdentity, SessionDemand, WithdrawResult, claim_result,
};

impl Fixture {
    /// Follows `gateway/<id>` as `credential`, returning the stream and its first update.
    pub(super) async fn follow(&mut self, credential: &str, id: &str) -> (Streaming<Update>, Update) {
        let subscription = gateway_topic(id, "test");
        let mut updates = self.client.subscribe(authorized(subscription, credential)).await.unwrap().into_inner();
        let first = next(&mut updates).await;
        (updates, first)
    }

    pub(super) async fn platform(
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

pub(super) fn login(connection: &str) -> ClaimArguments {
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

/// A move of the player to an `arena` session, queued under `operation`.
fn arena(operation: &str) -> MoveRequest {
    MoveRequest {
        operation_id: operation.into(),
        player_id: runtime::PLAYER.into(),
        demand: runtime::demand("arena"),
        ..MoveRequest::default()
    }
}

/// Reads `updates` until claim `key` has arrived, returning that update and the claim.
pub(super) async fn arrival(updates: &mut Streaming<Update>, key: &str) -> (Update, GatewayClaim) {
    loop {
        let update = next(updates).await;
        let arrived = update.upserts.iter().find_map(|entry| match &entry.state {
            Some(State::Value(value)) if entry.key == key => {
                Some(GatewayClaim::decode(&value[..]).unwrap()).filter(|claim| claim.phase() == ClaimPhase::Arrived)
            }
            _ => None,
        });
        if let Some(claim) = arrived {
            return (update, claim);
        }
    }
}

pub(super) fn result<T: Message + Default>(response: &CallResponse) -> T {
    match &response.outcome {
        Some(Outcome::Result(result)) => T::decode(result.as_slice()).unwrap(),
        outcome => panic!("expected a result, got {outcome:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_claims_activates_and_sees_its_player_arrive() {
    let (mut fixture, jvm) = with_jvm().await;
    let gateway = fixture.gateway.clone();
    let (mut updates, first) = fixture.follow(&gateway, "proxy").await;

    let claimed = fixture.platform(&gateway, &first.stream, "login", "chunk:claim", &login("connection")).await;
    let Some(claim_result::Outcome::Assignment(assignment)) = result::<ClaimResult>(&claimed).outcome else {
        panic!("expected an assignment");
    };
    assert_eq!(assignment.capability.len(), 32);
    let activated = fixture.platform(&gateway, &first.stream, "login", "chunk:activate", &()).await;
    assert!(!result::<ActivateResult>(&activated).waiting);

    let (_, claim) = arrival(&mut updates, "login").await;
    assert_eq!(claim.generation, assignment.generation);
    drop(updates);
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_proxy_claims_a_login_with_its_gateway_credential_and_sees_it_arrive() {
    let (fixture, jvm) = with_jvm().await;
    let target = chunk_proxy::PlatformTarget {
        core: fixture.endpoint.clone(),
        gateway: chunk_proxy::GatewayCredential { id: "proxy".into(), credential: fixture.gateway.clone() },
        deployment: "test".into(),
    };
    let login = chunk_proxy::testing::login(target, runtime::PLAYER, "player", runtime::gateway_demand("lobby"));
    let session = tokio::time::timeout(Duration::from_secs(30), login).await.unwrap().unwrap();
    assert!(!session.is_empty());
    fixture.stop().await;
    jvm.abort();
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
    let (mut fixture, jvm) = with_jvm().await;
    let gateway = fixture.gateway.clone();
    let (mut updates, first) = fixture.follow(&gateway, "proxy").await;
    let stream = first.stream.as_str();

    let claimed = fixture.platform(&gateway, stream, "login", "chunk:claim", &login("connection")).await;
    let replayed = fixture.platform(&gateway, stream, "login", "chunk:claim", &login("connection")).await;
    assert_eq!(result::<ClaimResult>(&replayed), result::<ClaimResult>(&claimed));
    let changed = fixture.platform(&gateway, stream, "login", "chunk:claim", &login("elsewhere")).await;
    assert_eq!(code(&changed), Code::OperationMismatch);
    let as_move = fixture.platform(&gateway, stream, "login", "chunk:claim", &ClaimArguments::default()).await;
    assert_eq!(code(&as_move), Code::OperationMismatch);

    // A login under a move's operation ID mismatches whether the move is queued or reserved.
    fixture.platform(&gateway, stream, "login", "chunk:activate", &()).await;
    arrival(&mut updates, "login").await;
    fixture.control.move_player(arena("move")).unwrap();
    let queued = fixture.platform(&gateway, stream, "move", "chunk:claim", &login("connection")).await;
    assert_eq!(code(&queued), Code::OperationMismatch);
    let moved = fixture.platform(&gateway, stream, "move", "chunk:claim", &ClaimArguments::default()).await;
    assert!(matches!(result::<ClaimResult>(&moved).outcome, Some(claim_result::Outcome::Assignment(_))));
    let reserved = fixture.platform(&gateway, stream, "move", "chunk:claim", &login("connection")).await;
    assert_eq!(code(&reserved), Code::OperationMismatch);

    let other = fixture.gateways.mint("other");
    let (foreign, stream) = fixture.follow(&other, "other").await;
    let withdrawn = fixture.platform(&other, &stream.stream, "login", "chunk:withdraw", &()).await;
    assert_eq!(code(&withdrawn), Code::Denied);
    drop((updates, foreign));
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_abandons_a_move_then_withdraws_its_arrived_claim_and_departs() {
    let (mut fixture, jvm) = with_jvm().await;
    let gateway = fixture.gateway.clone();
    let (mut updates, first) = fixture.follow(&gateway, "proxy").await;
    let stream = first.stream.as_str();
    fixture.platform(&gateway, stream, "login", "chunk:claim", &login("connection")).await;
    fixture.platform(&gateway, stream, "login", "chunk:activate", &()).await;
    arrival(&mut updates, "login").await;

    fixture.control.move_player(arena("move")).unwrap();
    let reason = AbandonMoveArguments { reason: "the destination refused the player".into() };
    let abandoned = fixture.platform(&gateway, stream, "move", "chunk:abandon_move", &reason).await;
    assert!(!result::<WithdrawResult>(&abandoned).unknown);

    let withdrawn = fixture.platform(&gateway, stream, "login", "chunk:withdraw", &()).await;
    assert!(!result::<WithdrawResult>(&withdrawn).unknown);
    while !next(&mut updates).await.removed.iter().any(|key| key == "login") {}
    let departed = fixture.platform(&gateway, stream, "login", "chunk:depart", &()).await;
    assert!(result::<DepartResult>(&departed).departed);
    drop(updates);
    fixture.stop().await;
    jvm.abort();
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
