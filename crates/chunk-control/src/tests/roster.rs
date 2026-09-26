use super::*;
use crate::{RosterMember, RosterMove};
use chunk_proto::v1::{ClaimIdentity, MovePlayerRequest};

/// Sessions of capacity 4, and one `arena` session at most.
async fn fixture() -> Fixture {
    let mut fixture = Fixture::new().await;
    let app: chunk_contract::AppArtifact = serde_json::from_value(serde_json::json!({"id":"bridge","jar":"app.jar",
        "sha256":"artifact","java_version":25,"sessions":{"default":{"machine_profile":"local","capacity":4}}}))
    .unwrap();
    fixture.config.apps.insert("bridge".into(), app);
    fixture.config.session_types.get_mut("bridge/default").unwrap().capacity = 4;
    fixture.config.max_processes = 4;
    fixture.config.contracts.destinations = Some(
        serde_json::from_value(serde_json::json!({"version":1,"entries":{"shared/destinations/arena":{
            "destination":{"key":"arena","session_type":"bridge/default","machine_profile":"local"},
            "overflow":"reject","empty_timeout_seconds":60}}}))
        .unwrap(),
    );
    fixture
}

fn arena() -> SessionDemand {
    SessionDemand { key: "arena".into(), session_type: "bridge/default".into(), machine_profile: "local".into() }
}

/// Logs a player into the lobby and returns its arrived claim.
async fn arrive(fixture: &Fixture, control: &Arc<Control>, operation: &str) -> (String, ClaimIdentity) {
    let player = uuid::Uuid::new_v4().to_string();
    let assignment = control.claim(request(operation, &player)).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut(operation).unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: assignment.claim.clone() }).await.unwrap();
    (player, assignment.claim.unwrap())
}

fn roster(operation: &str, version: u64, members: &[(String, ClaimIdentity)]) -> RosterMove {
    let members = members
        .iter()
        .map(|(player, source)| RosterMember {
            operation_id: format!("{operation}/{}", source.operation_id),
            player_id: player.clone(),
            expected_source: source.clone(),
            expected_connection_id: format!("connection-{}", source.operation_id),
        })
        .collect();
    RosterMove { operation_id: operation.into(), version, demand: arena(), members }
}

#[tokio::test]
async fn simultaneous_group_and_single_demand_never_split_a_roster_or_overfill_a_destination() {
    let fixture = fixture().await;
    let control = fixture.control();
    let mut players = Vec::new();
    for index in 0..6 {
        players.push(arrive(&fixture, &control, &format!("lobby-{index}")).await);
    }
    let group = roster("party", 1, &players[..3]);
    let mut tasks = tokio::task::JoinSet::new();
    let party = control.clone();
    tasks.spawn(async move { party.move_roster(&group).map(|_| ()) });
    for (player, source) in players[3..].iter().cloned() {
        let control = control.clone();
        tasks.spawn(async move {
            let destination = control.move_player(MovePlayerRequest {
                operation_id: format!("solo/{}", source.operation_id),
                player_id: player,
                demand: Some(arena()),
                expected_source: None,
                expected_connection_id: String::new(),
            })?;
            control.claim(destination).await.map(|_| ())
        });
    }
    while let Some(result) = tasks.join_next().await {
        assert!(matches!(result.unwrap(), Ok(()) | Err(Error::Capacity)));
    }
    let state = control.state().unwrap();
    let arena: Vec<_> =
        state.claims.iter().filter(|(_, claim)| state.sessions[&claim.session].demand_key == "arena").collect();
    assert!(arena.len() <= 4);
    let members: Vec<_> = arena.iter().filter(|(_, claim)| claim.roster.is_some()).collect();
    assert!(members.is_empty() || (members.len() == 3 && state.rosters.contains_key("party")));
    assert!(members.iter().all(|(_, claim)| claim.session == members[0].1.session));
    assert_eq!(state.players.len(), 6);
    for (player, source) in &players {
        let owner = &state.players[player];
        assert_eq!(owner.current.as_ref(), Some(&source.operation_id));
        assert_eq!(
            state.claims.values().filter(|claim| claim.player == *player && claim.phase != Phase::Released).count(),
            1 + usize::from(owner.pending.is_some())
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn a_roster_is_reserved_and_admitted_whole_or_fails_whole() {
    let fixture = fixture().await;
    let control = fixture.control();
    let (first, second, third) = (
        arrive(&fixture, &control, "a").await,
        arrive(&fixture, &control, "b").await,
        arrive(&fixture, &control, "c").await,
    );
    let mut outdated = roster("stale", 1, &[first.clone(), second.clone()]);
    outdated.members[1].expected_source.delivery_generation += 1;
    assert!(control.move_roster(&outdated).is_err());
    let state = control.state().unwrap();
    assert!(state.rosters.is_empty() && state.moves.is_empty() && state.players.values().all(|p| p.pending.is_none()));

    let pair = roster("pair", 1, &[first.clone(), second.clone()]);
    let destinations = control.move_roster(&pair).unwrap();
    assert_eq!(control.move_roster(&pair).unwrap(), destinations);
    assert!(
        control.move_roster(&RosterMove { version: 2, ..roster("pair", 1, &[first.clone(), second.clone()]) }).is_err()
    );
    let mut reconnected = roster("pair", 1, &[first.clone(), second.clone()]);
    reconnected.members[1].expected_connection_id = "connection-other".into();
    assert!(control.move_roster(&reconnected).is_err());
    let mut activations = Vec::new();
    for (destination, (player, source)) in destinations.iter().zip([&first, &second]) {
        let prepared = control.claim(destination.clone()).await.unwrap();
        control.cancel(request(&source.operation_id, player)).await.unwrap();
        activations.push(ActivateClaim { claim: prepared.claim });
    }
    // Gateways retry exactly this status until the group is complete.
    let service = crate::Service::new(control.clone(), "control-group-credential-with-32-characters".into()).unwrap();
    let mut waiting = Request::new(activations[0].clone());
    waiting
        .metadata_mut()
        .insert("authorization", "Bearer control-group-credential-with-32-characters".parse().unwrap());
    let status = chunk_proto::v1::local_control_server::LocalControl::activate(&service, waiting).await.unwrap_err();
    assert_eq!((status.code(), status.message()), (tonic::Code::Unavailable, "roster awaiting members"));
    assert!(
        control.state().unwrap().claims.values().filter(|claim| claim.roster.is_some()).all(|claim| !claim.activated)
    );
    control.activate(activations[1].clone()).await.unwrap();
    control.activate(activations[0].clone()).await.unwrap();
    assert!(control.state().unwrap().rosters["pair"].admitted);

    let fourth = arrive(&fixture, &control, "d").await;
    let group = roster("group", 1, &[third.clone(), fourth.clone()]);
    let destinations = control.move_roster(&group).unwrap();
    control.cancel(destinations[1].clone()).await.unwrap();
    let prepared = control.claim(destinations[0].clone()).await;
    assert!(prepared.is_err());
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    assert!(state.claims[&destinations[0].operation_id].phase == Phase::Released);
    assert_eq!(state.players[&third.0].current.as_ref(), Some(&third.1.operation_id));
    assert!(state.players[&third.0].pending.is_none());
    let failure = &state.moves[&destinations[0].operation_id].failure;
    assert_eq!(failure.as_ref().unwrap().reason, "roster member left");
    fixture.close().await;
}

#[tokio::test]
async fn canceling_a_roster_before_preparation_releases_its_reservations() {
    let fixture = fixture().await;
    let control = fixture.control();
    let members = [arrive(&fixture, &control, "a").await, arrive(&fixture, &control, "b").await];
    let destinations = control.move_roster(&roster("party", 1, &members)).unwrap();
    control.cancel_roster("party").unwrap();
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    for (destination, (player, source)) in destinations.iter().zip(&members) {
        assert!(state.claims[&destination.operation_id].phase == Phase::Released);
        assert_eq!(state.players[player].current.as_ref(), Some(&source.operation_id));
        assert!(state.players[player].pending.is_none());
    }
    fixture.close().await;
}
