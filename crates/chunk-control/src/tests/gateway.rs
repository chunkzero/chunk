use super::*;
use crate::{Generation, gateway::Topic, state::Claim};
use chunk_proto::sync::v1::{GatewayClaim, Update, entry::State};
use prost::Message;

/// Commits a reserved claim for each of `operations`, held through `gateway`.
fn add(control: &Control, gateway: &str, operations: impl IntoIterator<Item = String>) {
    control
        .update(|state| {
            for operation in operations {
                let claim = Claim {
                    request: request(&operation, &operation).encode_to_vec(),
                    player: operation.clone(),
                    proxy: gateway.into(),
                    membership: Generation::PENDING,
                    generation: Generation::PENDING,
                    session: String::new(),
                    phase: Phase::Reserved,
                    assignment: None,
                    activated: false,
                    created_at_ms: 0,
                    assigned_at_ms: None,
                    released_at_ms: None,
                    roster: None,
                };
                state.claims.insert(operation, claim);
            }
            Ok(())
        })
        .unwrap();
}

fn keys(update: &Update) -> Vec<&str> {
    update.upserts.iter().map(|entry| entry.key.as_str()).collect()
}

fn cursor(update: &Update) -> Generation {
    let position = update.position.unwrap();
    Generation { epoch: position.epoch, revision: position.revision }
}

/// Control with no capacity executor or JVM, so only the test commits.
fn bare(fixture: &Fixture) -> Arc<Control> {
    open(&fixture.directory.path().join("control.sqlite"), fixture.release.clone(), fixture.host.clone()).unwrap()
}

#[tokio::test]
async fn resuming_inside_retained_history_sends_only_changes_and_a_snapshot_beyond_it() {
    let fixture = Fixture::new();
    let control = bare(&fixture);
    add(&control, "gateway", ["kept".into(), "released".into()]);
    let (_, first) = Topic::open(&control, "gateway", None).unwrap();
    assert!(first.snapshot);
    assert_eq!(keys(&first), ["kept", "released"]);

    control
        .update(|state| {
            state.claims.get_mut("released").unwrap().phase = Phase::Released;
            Ok(())
        })
        .unwrap();
    add(&control, "gateway", ["joined".into()]);
    add(&control, "other", ["foreign".into()]);
    let (_, resumed) = Topic::open(&control, "gateway", Some(cursor(&first))).unwrap();
    assert!(!resumed.snapshot);
    assert_eq!(keys(&resumed), ["joined"]);
    assert_eq!(resumed.removed, ["released"]);
    assert_eq!(cursor(&resumed), control.state().unwrap().position());

    // One commit touching more claims than the feed retains leaves the earlier position behind.
    add(&control, "other", (0..4097).map(|index| format!("foreign-{index}")));
    let (_, gap) = Topic::open(&control, "gateway", Some(cursor(&resumed))).unwrap();
    assert!(gap.snapshot);
    assert_eq!(keys(&gap), ["joined", "kept"]);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_update_carries_the_state_at_its_position_while_commits_race() {
    const CLAIMS: usize = 200;
    let fixture = Fixture::new();
    let control = bare(&fixture);
    let mut positions = control.subscribe();
    let (mut topic, first) = Topic::open(&control, "gateway", None).unwrap();
    let writer = std::thread::spawn({
        let control = control.clone();
        move || (0..CLAIMS).for_each(|index| add(&control, "gateway", [format!("claim-{index}")]))
    });
    let (mut last, mut received) = (cursor(&first), 0);
    while received < CLAIMS {
        positions.borrow_and_update();
        if let Some(update) = topic.next(&control).unwrap() {
            let position = cursor(&update);
            assert!(position >= last, "positions never decrease");
            for entry in &update.upserts {
                let Some(State::Value(value)) = &entry.state else { panic!("a value") };
                let generation = GatewayClaim::decode(&value[..]).unwrap().generation.unwrap();
                assert!(generation.revision <= position.revision, "a value from after the update's position");
            }
            (last, received) = (position, received + update.upserts.len());
        }
        if received < CLAIMS {
            tokio::time::timeout(Duration::from_secs(5), positions.changed()).await.unwrap().unwrap();
        }
    }
    writer.join().unwrap();
    assert_eq!(last, control.state().unwrap().position());
    fixture.close().await;
}
