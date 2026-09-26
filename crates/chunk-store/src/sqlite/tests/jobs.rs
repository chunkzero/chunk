use super::*;
use crate::{Job, JobCommand, JobIntent, JobState};
use chunk_contract::{Contracts, Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};

fn target() -> Deployment {
    Deployment {
        contract_version: 2,
        runtime_profile: RuntimeProfile::TransactionalV1,
        contracts: Contracts::default(),
        id: "v1".into(),
        source: "export function work() { return null; }".into(),
        tables: crate::tests::schema(),
        functions: [(
            "work".into(),
            Function {
                kind: FunctionKind::Action,
                visibility: Visibility::Internal,
                export: "work".into(),
                arguments: Schema::Null,
                result: Schema::Null,
            },
        )]
        .into(),
    }
}
fn job(id: &str) -> Job {
    Job {
        id: id.into(),
        deployment: "v1".into(),
        function: "work".into(),
        arguments: json!(null),
        caller: json!({"player":"alice"}),
        due_at: 10,
        attempt: 1,
        state: JobState::Pending,
        result: None,
    }
}

#[test]
fn document_job_and_wake_roll_back_and_commit_as_one_batch() {
    let (_directory, mut store) = open();
    store.retain_deployment(&target()).unwrap();
    let intent = JobIntent::Schedule(job("one"));
    let batch = || commit("schedule", 1, vec![write("alice", Some(json!({"coins":7})))]);
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_outcome BEFORE INSERT ON _chunk_operations BEGIN SELECT RAISE(ABORT,'fixture'); END;",
        )
        .unwrap();
    assert!(store.commit_with_jobs(batch(), vec![intent.clone()]).is_err());
    assert!(store.jobs().unwrap().records.is_empty());
    assert_eq!(store.jobs().unwrap().wake.generation, 0);
    assert_eq!(totals(&store), (0, 0));
    assert!(store.outcome(&operation("schedule")).unwrap().is_none());
    store.connection.execute_batch("DROP TRIGGER fail_outcome;").unwrap();
    let outcome = store.commit_with_jobs(batch(), vec![intent.clone()]).unwrap();
    assert_eq!(store.commit_with_jobs(batch(), vec![intent]).unwrap(), outcome);
    let jobs = store.jobs().unwrap();
    assert_eq!(jobs.records.len(), 1);
    assert_eq!(jobs.wake.generation, 1);
    assert_eq!(jobs.wake.due_at, Some(10));
    assert!(!jobs.wake.acknowledged);
    assert_eq!(totals(&store), (1, 11));
    assert!(store.release_deployment("v1").is_err());
}

#[test]
fn claims_recover_unknown_with_stable_attempts_and_checked_owner_retry_retention_and_wake() {
    let (directory, mut store) = open();
    store.retain_deployment(&target()).unwrap();
    store
        .commit_with_jobs(
            commit("schedule", 1, vec![]),
            vec![JobIntent::Schedule(job("one")), JobIntent::Schedule(job("pending"))],
        )
        .unwrap();
    let wake = store.jobs().unwrap().wake;
    assert!(store.job_command(JobCommand::AcknowledgeWake { generation: wake.generation, due_at: Some(11) }).is_err());
    assert!(
        store
            .job_command(JobCommand::AcknowledgeWake { generation: wake.generation, due_at: wake.due_at })
            .unwrap()
            .wake
            .acknowledged
    );
    assert_eq!(store.job_command(JobCommand::Claim { id: "one".into(), attempt: 1, now: 10 }).unwrap().wake.running, 1);
    assert!(store.job_command(JobCommand::Forget { id: "one".into(), caller: job("one").caller }).is_err());
    drop(store);
    let mut store = SqliteStore::open(directory.path().join("data.db"), "local").unwrap();
    let recovered = store.job_command(JobCommand::Recover).unwrap();
    assert_eq!(recovered.wake.running, 0);
    let one = recovered.records.iter().find(|job| job.id == "one").unwrap();
    assert_eq!(one.state, JobState::Unknown);
    assert_eq!(one.invocation_id(), "job/one/attempt/1");
    assert_eq!(recovered.records.iter().find(|job| job.id == "pending").unwrap().state, JobState::Pending);
    assert!(
        store.job_command(JobCommand::AcknowledgeWake { generation: wake.generation, due_at: wake.due_at }).is_err()
    );
    let retry = |caller, acknowledge_possible_effects| JobIntent::Retry {
        id: "one".into(),
        caller,
        due_at: 20,
        acknowledge_possible_effects,
    };
    assert!(store.commit_with_jobs(commit("bad-owner", 2, vec![]), vec![retry(json!(null), true)]).is_err());
    assert!(store.commit_with_jobs(commit("bad-ack", 2, vec![]), vec![retry(job("one").caller, false)]).is_err());
    store
        .commit_with_jobs(
            commit("retry", 2, vec![]),
            vec![retry(job("one").caller, true), JobIntent::Cancel { id: "pending".into(), caller: job("one").caller }],
        )
        .unwrap();
    let jobs = store.jobs().unwrap();
    assert_eq!(jobs.records.iter().find(|job| job.id == "one").unwrap().invocation_id(), "job/one/attempt/2");
    assert_eq!(jobs.wake.due_at, Some(20));
    store
        .commit_with_jobs(
            commit("cancel", 3, vec![]),
            vec![JobIntent::Cancel { id: "one".into(), caller: job("one").caller }],
        )
        .unwrap();
    assert!(store.release_deployment("v1").unwrap());
    assert!(store.commit_with_jobs(commit("released-retry", 4, vec![]), vec![retry(job("one").caller, true)]).is_err());
    let jobs = store.job_command(JobCommand::Forget { id: "one".into(), caller: job("one").caller }).unwrap();
    assert_eq!(jobs.records.len(), 1);
    assert_eq!(jobs.wake.due_at, None);
}

#[test]
fn full_job_queue_rejects_new_intent_and_its_document_writes() {
    let (_directory, mut store) = open();
    store.retain_deployment(&target()).unwrap();
    for batch in 0..16 {
        let intents = (0..16).map(|index| JobIntent::Schedule(job(&format!("{batch}-{index}")))).collect();
        store.commit_with_jobs(commit(&format!("batch-{batch}"), batch + 1, vec![]), intents).unwrap();
    }
    assert!(matches!(
        store.commit_with_jobs(
            commit("overflow", 17, vec![write("alice", Some(json!({"coins":7})))]),
            vec![JobIntent::Schedule(job("overflow"))]
        ),
        Err(Error::Capacity)
    ));
    assert_eq!(store.jobs().unwrap().records.len(), 256);
    assert_eq!(totals(&store), (0, 0));
    assert!(store.outcome(&operation("overflow")).unwrap().is_none());
}

#[test]
fn exhausted_result_retention_still_records_terminal_state() {
    let (_directory, mut store) = open();
    let mut deployment = target();
    deployment.functions.get_mut("work").unwrap().arguments = Schema::String;
    store.retain_deployment(&deployment).unwrap();
    for batch in 0..4 {
        let intents = (0..if batch == 3 { 15 } else { 16 })
            .map(|index| {
                let mut job = job(&format!("{batch}-{index}"));
                job.caller = json!("x".repeat(64 * 1024 - 2));
                job.arguments = job.caller.clone();
                JobIntent::Schedule(job)
            })
            .collect();
        store.commit_with_jobs(commit(&format!("batch-{batch}"), batch + 1, vec![]), intents).unwrap();
    }
    for id in ["0-0", "0-1"] {
        store.job_command(JobCommand::Claim { id: id.into(), attempt: 1, now: 10 }).unwrap();
        store
            .job_command(JobCommand::Finish {
                id: id.into(),
                attempt: 1,
                state: JobState::Succeeded,
                result: Some(json!("x".repeat(64 * 1024 - 2))),
            })
            .unwrap();
    }
    let jobs = store.jobs().unwrap();
    assert_eq!(jobs.records.iter().find(|job| job.id == "0-0").unwrap().state, JobState::Succeeded);
    let full = jobs.records.iter().find(|job| job.id == "0-1").unwrap();
    assert_eq!(full.state, JobState::Failed);
    assert!(full.result.is_none());
}

#[test]
fn finished_jobs_expire_on_later_job_commands() {
    let (_directory, mut store) = open();
    store.retain_deployment(&target()).unwrap();
    let intents = vec![JobIntent::Schedule(job("done")), JobIntent::Schedule(job("waiting"))];
    store.commit_with_jobs(commit("schedule", 1, vec![]), intents).unwrap();
    store.job_command(JobCommand::Claim { id: "done".into(), attempt: 1, now: 10 }).unwrap();
    let finish = JobCommand::Finish { id: "done".into(), attempt: 1, state: JobState::Succeeded, result: None };
    assert_eq!(store.job_command(finish).unwrap().records.len(), 2);

    std::thread::sleep(std::time::Duration::from_millis(5));
    store.set_retention(crate::Retention { jobs: std::time::Duration::ZERO, ..crate::Retention::default() });
    let generation = store.jobs().unwrap().wake.generation;
    let jobs = store.job_command(JobCommand::Recover).unwrap();
    assert_eq!(jobs.records.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(), ["waiting"]);
    assert!(jobs.wake.generation > generation);
}
