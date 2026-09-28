use super::*;

#[tokio::test]
async fn queued_moves_reach_the_watching_proxy_without_polling() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, "source").await;
    control.activate(first.claim.clone().unwrap()).await.unwrap();
    let mut positions = control.subscribe();
    let (mut view, snapshot) = View::open(&control, "proxy-1", None).unwrap();
    let [(operation, initial)] = &snapshot.upserts[..] else { panic!("expected one claim") };
    assert!(snapshot.snapshot && operation == "source" && initial.pending_move.is_none());
    assert!(initial.phase == Phase::Arrived && initial.generation.wire() == first.claim.unwrap().delivery_generation);

    let destination = control
        .move_player(MoveRequest {
            operation_id: "move".into(),
            player_id: uuid,
            demand: SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() },
            source: None,
        })
        .unwrap();
    let update = changed(&control, &mut view, &mut positions).await;
    let [(operation, moved)] = &update.upserts[..] else { panic!("expected one claim") };
    assert!(!update.snapshot && operation == "source");
    assert_eq!(moved.pending_move, Some(destination));
    fixture.close().await;
}
