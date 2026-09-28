use super::{super::MESSAGE_BYTES, *};
use chunk_proto::sync::v1::entry::State;

fn upsert(key: &str, value: &str, revision: u64) -> Update {
    Update {
        position: Some(Position { epoch: 1, revision }),
        upserts: vec![Entry { key: key.into(), state: Some(State::Value(value.as_bytes().to_vec())) }],
        ..Update::default()
    }
}

#[test]
fn merged_changes_keep_every_key_at_its_latest_value() {
    let mut changes = Changes::default();
    changes.merge(upsert("a", "1", 2));
    changes.merge(upsert("b", "1", 3));
    changes.merge(upsert("a", "2", 4));
    changes.merge(Update {
        position: Some(Position { epoch: 1, revision: 5 }),
        removed: vec!["b".into()],
        ..Update::default()
    });

    let update = changes.into_update();
    assert_eq!(update.position, Some(Position { epoch: 1, revision: 5 }));
    let values: Vec<_> = update.upserts.iter().map(|entry| (entry.key.as_str(), entry.state.clone())).collect();
    assert_eq!(values, [("a", Some(State::Value(b"2".to_vec())))]);
    assert_eq!(update.removed, ["b"]);
}

#[tokio::test]
async fn ending_a_stalled_stream_releases_what_its_client_has_yet_to_take() {
    let (sender, mut stream) = channel(SendBudget::new(MESSAGE_BYTES));
    sender.send(upsert("a", "1", 1));
    while stream.parts.is_empty() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    sender.send(upsert("b", "1", 2));
    sender.end(errors::invalid("replaced"));
    let released = async {
        while !stream.parts.is_closed() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(1), released).await.expect("the writer ended without a reader");
    assert_eq!(stream.parts.recv().await.unwrap().update.upserts[0].key, "a");
    assert!(stream.parts.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn position_only_updates_are_paced_but_data_is_not() {
    let (sender, mut stream) = channel(SendBudget::new(MESSAGE_BYTES));
    let mut received = 0;
    for revision in 1..=500 {
        sender.send(Update { position: Some(Position { epoch: 1, revision }), ..Update::default() });
        tokio::time::sleep(Duration::from_millis(1)).await;
        while stream.parts.try_recv().is_ok() {
            received += 1;
        }
    }
    assert!((10..=11).contains(&received), "{received} advances in 500 ms");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let held = stream.parts.recv().await.unwrap().update;
    assert_eq!(held.position, Some(Position { epoch: 1, revision: 500 }));

    let sent = Instant::now();
    sender.send(upsert("a", "1", 501));
    let update = stream.parts.recv().await.unwrap().update;
    assert_eq!(Instant::now(), sent);
    assert_eq!((update.upserts.len(), update.position), (1, Some(Position { epoch: 1, revision: 501 })));
}

#[tokio::test(start_paused = true)]
async fn changes_the_budget_has_no_room_for_keep_coalescing_and_are_retried() {
    let budget = SendBudget::new(1024);
    let filler = budget.charge(1024).unwrap();
    let (sender, mut stream) = channel(budget.clone());
    sender.send(upsert("a", "1", 1));
    tokio::time::sleep(Duration::from_secs(1)).await;
    sender.send(upsert("b", "1", 2));
    sender.send(upsert("b", "2", 3));
    sender.send(upsert("c", "1", 4));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(stream.parts.try_recv().is_err());

    drop(filler);
    let freed = Instant::now();
    let part = stream.parts.recv().await.unwrap();
    assert!(freed.elapsed() <= RETRY);
    assert_eq!(part.charge.as_ref().map(SendCharge::bytes), Some(part.update.encoded_len() + PREFIX_BYTES));
    let values: Vec<_> = part.update.upserts.iter().map(|entry| (entry.key.as_str(), entry.state.clone())).collect();
    let value = |value: &str| Some(State::Value(value.as_bytes().to_vec()));
    assert_eq!(values, [("a", value("1")), ("b", value("2")), ("c", value("1"))]);
    assert_eq!(part.update.position, Some(Position { epoch: 1, revision: 4 }));
    drop(part);
    assert_eq!(budget.bytes(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_stream_the_budget_has_no_room_for_until_its_deadline_ends_overloaded() {
    let budget = SendBudget::new(1024);
    let filler = budget.charge(1024).unwrap();
    let (sender, mut stream) = channel(budget.clone());
    sender.send(upsert("a", "1", 1));
    let refused = Instant::now();
    tokio::time::sleep(DEADLINE / 2).await;
    sender.send(upsert("b", "1", 2));
    let last = stream.parts.recv().await.unwrap();
    assert!((DEADLINE..DEADLINE + RETRY).contains(&refused.elapsed()));
    assert_eq!(last.update.error.as_ref().map(Error::code), Some(Code::Overloaded));
    assert_eq!(last.charge.as_ref().map(SendCharge::bytes), Some(last.update.encoded_len() + PREFIX_BYTES));
    assert!(stream.parts.recv().await.is_none());
    drop((filler, last));
    assert_eq!(budget.bytes(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_value_the_budget_has_no_room_for_gives_way_to_a_newer_one_that_fits() {
    let budget = SendBudget::new(1024);
    let (sender, mut stream) = channel(budget.clone());
    sender.send(upsert("a", &"x".repeat(4096), 1));
    let refused = Instant::now();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(stream.parts.try_recv().is_err());

    sender.send(upsert("a", "1", 2));
    let part = stream.parts.recv().await.unwrap();
    assert!(refused.elapsed() < DEADLINE);
    let values: Vec<_> = part.update.upserts.iter().map(|entry| (entry.key.as_str(), entry.state.clone())).collect();
    assert_eq!(values, [("a", Some(State::Value(b"1".to_vec())))]);
    assert_eq!(part.update.position, Some(Position { epoch: 1, revision: 2 }));
    drop(part);
    assert_eq!(budget.bytes(), 0);
}
