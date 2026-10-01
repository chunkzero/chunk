//! Moves a JVM asks for, fenced to the claim of the player it holds.

use super::*;
use chunk_contract::MoveRefusal;

#[tokio::test]
async fn a_jvm_moves_only_players_it_hosts_under_their_current_generation() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let source = request("source", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, "source").await;
    control.activate(assignment.claim.clone().unwrap()).await.unwrap();
    let generation = Generation::from_wire(assignment.claim.unwrap().delivery_generation);
    let host = {
        let state = control.state().unwrap();
        state.sessions[&state.claims["source"].session].host.clone()
    };
    let arena = SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() };

    let foreign = control.move_hosted("another-host", "move", "source", generation, arena.clone());
    assert!(matches!(foreign, Err(Error::Invalid(crate::NOT_HOSTED))), "{foreign:?}");
    let older = Generation { revision: generation.revision - 1, ..generation };
    // Pairs that pack into the current generation's wire form are still other generations.
    let carried = Generation { epoch: generation.epoch - 1, revision: generation.revision + (1 << 40) };
    let oversized = Generation { epoch: generation.epoch + (1 << 24), ..generation };
    assert_eq!((carried.wire(), oversized.wire()), (generation.wire(), generation.wire()));
    for named in [older, carried, oversized] {
        let stale = control.move_hosted(&host, "move", "source", named, arena.clone());
        assert!(matches!(stale, Err(Error::Refused(MoveRefusal::Stale))), "{named:?}: {stale:?}");
    }
    assert!(pending_move(&control, &source).is_none());

    control.move_hosted(&host, "move", "source", generation, arena).unwrap();
    assert_eq!(pending_move(&control, &source).map(|moved| moved.operation_id).as_deref(), Some("move"));
    fixture.close().await;
}
