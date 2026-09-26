use super::*;
use crate::state::MoveIntent;
use chunk_proto::v1::{ClaimIdentity, MovePlayerRequest};
use prost::Message;

#[tokio::test]
async fn released_claims_and_their_moves_are_forgotten_after_retention_and_stay_forgotten() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
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

    let state = fixture.control().state().unwrap();
    assert!(!state.claims.contains_key("old") && state.moves.is_empty());
    assert!(state.claims["recent"].phase == Phase::Released);
    fixture.close().await;
}

#[tokio::test]
async fn released_claims_stay_while_an_open_claim_of_their_move_references_them() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("source").unwrap().phase = DeliveryPhase::Arrived;
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
    assert!(control.poll_move(&source).unwrap().claim.is_none());

    let destination = control.move_player(command("moved")).unwrap();
    let second = control.claim(destination).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("moved").unwrap().phase = DeliveryPhase::Arrived;
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
    let control = fixture.control();
    let source = request("source", &uuid::Uuid::new_v4().to_string());
    let first = control.claim(source.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("source").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: first.claim.clone() }).await.unwrap();
    let destination = ClaimRequest {
        operation_id: "direct".into(),
        demand: Some(SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() }),
        source: first.claim,
        ..source.clone()
    };
    let second = control.claim(destination).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("direct").unwrap().phase = DeliveryPhase::Arrived;
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
