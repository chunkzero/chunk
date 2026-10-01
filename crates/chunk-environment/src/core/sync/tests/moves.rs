//! Moves gameplay asks for, a JVM's `chunk:move` and an action's, each carried out by the player's gateway.

use super::{runtime::with_jvm, *};
use chunk_proto::sync::v1::{ClaimPhase, JvmMove, JvmMoveResult, MoveRefusal, OperatorPlayer};

impl Fixture {
    /// Asks, as the fake JVM, to move the player its `delivery` holds at `generation` to a `key` session.
    async fn jvm_move(&mut self, operation: &str, delivery: &str, generation: Position, key: &str) -> MoveRefusal {
        let arguments = JvmMove {
            delivery: delivery.into(),
            generation: Some(generation),
            destination: Some(runtime::gateway_demand(key)),
        };
        let message = CallRequest {
            operation_id: operation.into(),
            method: "chunk:move".into(),
            arguments: arguments.encode_to_vec(),
            ..CallRequest::default()
        };
        let response = self.client.call(authorized(message, JVM)).await.unwrap().into_inner();
        commands::decoded::<JvmMoveResult>(&response).refusal()
    }

    /// Carries out the move queued under `operation` from claim `source` as the gateway does, until the player arrived
    /// in a `key` session.
    async fn carry_out(&self, source: &str, operation: &str, key: &str) {
        let claim = |operation| self.control.stored_claim(operation).unwrap().expect("a claim").request;
        let assignment = self.control.claim(claim(operation)).await.unwrap();
        self.control.cancel(claim(source)).await.unwrap();
        self.control.activate(assignment.claim.unwrap()).await.unwrap();
        let arrived = |player: &OperatorPlayer| {
            player.phase() == ClaimPhase::Arrived && player.demand.as_ref().is_some_and(|demand| demand.key == key)
        };
        let landed = async {
            while !runtime::players(&self.control).iter().any(arrived) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), landed).await.expect("the player arrived");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_and_an_action_each_move_the_player_into_their_destinations_session() {
    let (mut fixture, jvm) = with_jvm().await;
    fixture.arrive().await;
    let claim = fixture.control.stored_claim("login").unwrap().and_then(|claim| claim.identity).unwrap();
    let generation = chunk_control::Generation::from_wire(claim.delivery_generation);
    let current = Position { epoch: generation.epoch, revision: generation.revision };
    let older = Position { revision: current.revision - 1, ..current };
    assert_eq!(fixture.jvm_move("jvm-move", "login", older, "arena").await, MoveRefusal::Stale);
    assert_eq!(fixture.jvm_move("jvm-move", "login", current, "arena").await, MoveRefusal::Unspecified);
    fixture.carry_out("login", "jvm-move", "arena").await;

    let cli = fixture.cli.clone();
    let operation = fixture.prepare(&cli).await;
    let moved = fixture.call(&cli, &operation, "relocate", r#""lobby""#).await;
    let Some(Outcome::Result(moved)) = moved.outcome else { panic!("expected a result, got {moved:?}") };
    let moved: String = serde_json::from_slice(&moved).unwrap();
    assert!(moved.starts_with("action/"), "{moved}");
    fixture.carry_out("jvm-move", &moved, "lobby").await;
    fixture.stop().await;
    jvm.abort();
}
