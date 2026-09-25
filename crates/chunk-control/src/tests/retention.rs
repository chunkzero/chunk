use super::*;
use crate::state::MoveIntent;
use chunk_proto::v1::ClaimIdentity;
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
