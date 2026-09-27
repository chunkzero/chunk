use super::*;
use chunk_proto::v1::MovePlayerRequest;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn queued_moves_reach_the_watching_proxy_without_polling() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, "source").await;
    control.activate(ActivateClaim { claim: first.claim.clone() }).await.unwrap();
    let (sender, mut updates) = tokio::sync::mpsc::channel(1);
    let proxy = control.clone();
    let stream = tokio::spawn(async move { proxy.watch("proxy-1".into(), sender, CancellationToken::new()).await });
    let snapshot = updates.recv().await.unwrap().unwrap();
    let [initial] = snapshot.claims.try_into().unwrap();
    assert!(snapshot.snapshot && initial.claim == first.claim && initial.pending_move.is_none());
    assert_eq!(initial.phase, ClaimPhase::Arrived as i32);

    let destination = control
        .move_player(MovePlayerRequest {
            expected_source: None,
            expected_connection_id: String::new(),
            operation_id: "move".into(),
            player_id: uuid,
            demand: Some(SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() }),
        })
        .unwrap();
    let update = tokio::time::timeout(Duration::from_secs(1), updates.recv()).await.unwrap().unwrap().unwrap();
    let [moved] = update.claims.try_into().unwrap();
    assert!(!update.snapshot && moved.claim == first.claim);
    assert_eq!(moved.pending_move, Some(destination));
    stream.abort();
    fixture.close().await;
}
