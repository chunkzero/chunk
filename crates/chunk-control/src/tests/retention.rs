use super::*;
use crate::state::MoveIntent;
use chunk_proto::v1::{ClaimIdentity, MovePlayerRequest};
use prost::Message;

#[tokio::test]
async fn released_claims_and_their_moves_are_forgotten_after_retention_and_stay_forgotten() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    for operation in ["old", "recent"] {
        let claim = request(operation, &uuid::Uuid::new_v4().to_string());
        control.claim(claim.clone()).await.unwrap();
        control.cancel(claim).await.unwrap();
    }
    let mut destination = request("moved", &uuid::Uuid::new_v4().to_string());
    destination.source = Some(ClaimIdentity { operation_id: "old".into(), ..Default::default() });
    control
        .update(|state| {
            state.claims.get_mut("old").unwrap().released_at_ms = Some(0);
            let intent =
                MoveIntent { request: destination.encode_to_vec(), canceled: false, sequence: 1, failure: None };
            state.moves.insert("moved".into(), intent);
            Ok(())
        })
        .unwrap();
    control.retire_idle_hosts().unwrap();
    drop(control);
    fixture.detach().await;

    let reopened = open(&fixture.directory.path().join("control.sqlite"), fixture.config.clone(), fixture.host.clone());
    let state = reopened.unwrap().state().unwrap();
    assert!(!state.claims.contains_key("old") && state.moves.is_empty());
    assert!(state.claims["recent"].phase == Phase::Released);
    fixture.close().await;
}

#[tokio::test]
async fn released_claims_stay_while_an_open_claim_of_their_move_references_them() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, "source").await;
    control.activate(ActivateClaim { claim: first.claim }).await.unwrap();
    let command = |operation: &str| MovePlayerRequest {
        expected_source: None,
        expected_connection_id: String::new(),
        operation_id: operation.into(),
        player_id: uuid.clone(),
        demand: Some(SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() }),
    };
    let expire = |operation: &str| {
        control
            .update(|state| {
                state.claims.get_mut(operation).unwrap().released_at_ms = Some(0);
                Ok(())
            })
            .unwrap();
        control.retire_idle_hosts().unwrap();
    };

    let canceled = control.move_player(command("canceled")).unwrap();
    control.claim(canceled.clone()).await.unwrap();
    control.cancel(canceled).await.unwrap();
    expire("canceled");
    assert!(pending_move(&control, &source).await.is_none());

    let destination = control.move_player(command("moved")).unwrap();
    let second = control.claim(destination).await.unwrap();
    fixture.arrive(&control, "moved").await;
    control.cancel(source.clone()).await.unwrap();
    let activation = ActivateClaim { claim: second.claim };
    control.activate(activation.clone()).await.unwrap();
    expire("source");
    control.activate(activation).await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn released_sources_stay_while_a_direct_destination_claim_is_open() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let source = request("source", &uuid::Uuid::new_v4().to_string());
    let first = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, "source").await;
    control.activate(ActivateClaim { claim: first.claim.clone() }).await.unwrap();
    let destination = ClaimRequest {
        operation_id: "direct".into(),
        demand: Some(SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() }),
        source: first.claim,
        ..source.clone()
    };
    let second = control.claim(destination).await.unwrap();
    fixture.arrive(&control, "direct").await;
    control.cancel(source).await.unwrap();
    let activation = ActivateClaim { claim: second.claim };
    control.activate(activation.clone()).await.unwrap();
    control
        .update(|state| {
            state.claims.get_mut("source").unwrap().released_at_ms = Some(0);
            Ok(())
        })
        .unwrap();
    control.retire_idle_hosts().unwrap();
    control.activate(activation).await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn players_are_forgotten_on_release_and_expired_claims_are_pruned() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let player = uuid::Uuid::new_v4().to_string();
    let claim = request("only", &player);
    control.claim(claim.clone()).await.unwrap();
    assert!(control.state().unwrap().players.contains_key(&player));
    control.cancel(claim).await.unwrap();
    assert!(!control.state().unwrap().players.contains_key(&player));
    control
        .update(|state| {
            let expired = crate::state::Claim { released_at_ms: Some(0), ..state.claims["only"].clone() };
            for index in 0..300 {
                state.claims.insert(format!("expired-{index}"), expired.clone());
            }
            Ok(())
        })
        .unwrap();
    control.retire_idle_hosts().unwrap();
    drop(control);
    let state = fixture.control().await.state().unwrap();
    assert_eq!(state.claims.keys().collect::<Vec<_>>(), ["only"]);
    fixture.close().await;
}

#[tokio::test]
async fn open_claims_are_unbounded_and_only_in_flight_work_is_refused_as_busy() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    control.claim(request("template", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    control
        .update(|state| {
            let open = crate::state::Claim { session: "elsewhere".into(), ..state.claims["template"].clone() };
            for index in 0..1100 {
                state.claims.insert(format!("open-{index}"), open.clone());
            }
            Ok(())
        })
        .unwrap();
    let held: Vec<_> = (0..1024).map(|index| control.operation(&format!("held-{index}")).unwrap()).collect();
    let next = request("next", &uuid::Uuid::new_v4().to_string());
    assert!(matches!(control.claim(next.clone()).await, Err(Error::Busy)));
    drop(held);
    control.claim(next).await.unwrap();
    assert!(control.state().unwrap().claims.values().filter(|claim| claim.phase != Phase::Released).count() > 1100);
    fixture.close().await;
}
