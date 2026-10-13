//! Moves a JVM asks for, fenced to the claim of the player it holds, and moves whose destination retires.

use super::*;
use chunk_contract::MoveRefusal;
use chunk_proto::control::v1::ClaimIdentity;

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

/// Logs `player` in as `source`, queues their move to an arena as `move` and reserves it.
async fn moving(fixture: &Fixture, control: &Control, player: &str) -> (ClaimRequest, ClaimRequest, ClaimIdentity) {
    let source = request("source", player);
    let assignment = control.claim(source.clone()).await.unwrap();
    fixture.arrive(control, "source").await;
    control.activate(assignment.claim.unwrap()).await.unwrap();
    let demand = SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() };
    let command = MoveRequest { operation_id: "move".into(), player_id: player.into(), demand, source: None };
    let destination = control.move_player(command).unwrap();
    let identity = control.claim(destination.clone()).await.unwrap().claim.unwrap();
    (source, destination, identity)
}

/// Retires the session of `operation`'s claim, leaving the claim in `phase`.
fn retire(control: &Control, operation: &str, phase: Phase) {
    control
        .update(|state| {
            let claim = state.claims.get_mut(operation).unwrap();
            claim.phase = phase;
            state.sessions.get_mut(&claim.session).unwrap().retired = true;
            Ok(())
        })
        .unwrap();
}

fn retired<T>(result: &Result<T>) -> bool {
    matches!(result, Err(Error::Invalid(crate::DESTINATION_RETIRED)))
}

#[tokio::test]
async fn a_move_into_a_session_that_retires_before_cutover_fails_and_leaves_the_source_active() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let (source, destination, identity) = moving(&fixture, &control, &uuid).await;
    retire(&control, "move", Phase::Reserved);

    assert!(retired(&control.claim(destination.clone()).await));
    assert!(retired(&control.activate(identity.clone()).await));
    // A claim named at another generation is stale, whatever became of its session.
    let older = ClaimIdentity { delivery_generation: identity.delivery_generation - 1, ..identity };
    let outdated = control.activate(older).await;
    assert!(matches!(outdated, Err(Error::Invalid(_))) && !retired(&outdated), "{outdated:?}");

    control.abandon_move(destination, crate::DESTINATION_RETIRED.into()).await.unwrap();
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    assert!(state.claims["move"].phase == Phase::Released);
    assert_eq!(state.players[&uuid].current.as_deref(), Some("source"));
    assert!(state.players[&uuid].pending.is_none());
    assert_eq!(control.inspect(&source).unwrap().phase, ClaimPhase::Arrived as i32);
    assert_eq!(players(&control)[0].1.last_move_failure.as_ref().unwrap().reason, crate::DESTINATION_RETIRED);
    fixture.close().await;
}

#[tokio::test]
async fn a_destination_that_retires_after_its_source_was_fenced_frees_the_player_to_log_in() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let (source, destination, identity) = moving(&fixture, &control, &uuid).await;
    control.cancel(source).await.unwrap();

    // A claim canceled while its session still runs is stale, and stays so when the session retires afterwards.
    control.cancel(destination).await.unwrap();
    let stale = control.activate(identity.clone()).await;
    assert!(matches!(stale, Err(Error::Invalid(_))) && !retired(&stale), "{stale:?}");
    retire(&control, "move", Phase::Released);
    let canceled = control.activate(identity).await;
    assert!(matches!(canceled, Err(Error::Invalid(_))) && !retired(&canceled), "{canceled:?}");

    assert!(!control.state().unwrap().players.contains_key(&uuid));
    control.claim(request("again", &uuid)).await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn reconciliation_ending_a_claim_in_a_retired_session_is_reported_until_a_newer_claim_owns_the_player() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let (source, destination, identity) = moving(&fixture, &control, &uuid).await;
    control.cancel(source).await.unwrap();
    retire(&control, "move", Phase::Reserved);
    control.reconcile_all().await.unwrap();
    let claim = control.state().unwrap().claims["move"].clone();
    assert!(!claim.canceled, "{:?}", claim.phase as u8);
    assert!(retired(&control.activate(identity.clone()).await));

    control.claim(request("again", &uuid)).await.unwrap();
    let superseded = control.activate(identity).await;
    assert!(matches!(superseded, Err(Error::Invalid(_))) && !retired(&superseded), "{superseded:?}");
    drop(destination);
    fixture.close().await;
}
