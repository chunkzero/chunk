use super::*;
use crate::tests::{job, target};
use crate::{JobCommand, JobIntent, JobLimits, JobState};
use chunk_contract::Schema;

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
fn cancelling_a_deployments_jobs_ends_its_pending_and_running_ones() {
    let (_directory, mut store) = open();
    store.retain_deployment(&target()).unwrap();
    let intents = vec![JobIntent::Schedule(job("running")), JobIntent::Schedule(job("pending"))];
    store.commit_with_jobs(commit("schedule", 1, vec![]), intents).unwrap();
    store.job_command(JobCommand::Claim { id: "running".into(), attempt: 1, now: 10 }).unwrap();
    let jobs = store.job_command(JobCommand::CancelDeployment { deployment: job("running").deployment }).unwrap();
    let state = |id| jobs.records.iter().find(|job| job.id == id).unwrap().state;
    assert_eq!((state("running"), state("pending")), (JobState::Unknown, JobState::Cancelled));
    assert_eq!(store.retiring().unwrap(), ["v1"]);
    assert!(store.release_deployment("v1").unwrap());
    assert!(store.retiring().unwrap().is_empty());
}

#[test]
fn retrying_a_job_of_a_retiring_deployment_leaves_it_cancelled() {
    let (_directory, mut store) = open();
    store.retain_deployment(&target()).unwrap();
    let owner = job("one").caller;
    store.commit_with_jobs(commit("schedule", 1, vec![]), vec![JobIntent::Schedule(job("one"))]).unwrap();
    store
        .commit_with_jobs(
            commit("cancel", 2, vec![]),
            vec![JobIntent::Cancel { id: "one".into(), caller: owner.clone() }],
        )
        .unwrap();
    store.job_command(JobCommand::CancelDeployment { deployment: job("one").deployment }).unwrap();
    store
        .commit_with_jobs(
            commit("retry", 3, vec![]),
            vec![JobIntent::Retry { id: "one".into(), caller: owner, due_at: 20, acknowledge_possible_effects: true }],
        )
        .unwrap();
    assert_eq!(store.jobs().unwrap().records[0].state, JobState::Cancelled);
    assert!(store.release_deployment("v1").unwrap());
}

#[test]
fn full_job_budget_rejects_new_intent_and_its_document_writes() {
    let (_directory, mut store) = open();
    store.retain_deployment(&target()).unwrap();
    store.set_job_limits(JobLimits { jobs: 32, ..JobLimits::default() });
    for batch in 0..2 {
        let intents = (0..16).map(|index| JobIntent::Schedule(job(&format!("{batch}-{index}")))).collect();
        store.commit_with_jobs(commit(&format!("batch-{batch}"), batch + 1, vec![]), intents).unwrap();
    }
    let overflow = || {
        (
            commit("overflow", 3, vec![write("alice", Some(json!({"coins":7})))]),
            vec![JobIntent::Schedule(job("overflow"))],
        )
    };
    let (first, intents) = overflow();
    assert!(matches!(store.commit_with_jobs(first, intents), Err(Error::JobBudget)));
    assert_eq!(store.jobs().unwrap().records.len(), 32);
    assert_eq!(totals(&store), (0, 0));
    assert!(store.outcome(&operation("overflow")).unwrap().is_none());

    // A lower budget keeps existing jobs readable; a higher one admits more.
    store.set_job_limits(JobLimits { jobs: 1, ..JobLimits::default() });
    assert_eq!(store.jobs().unwrap().records.len(), 32);
    store.set_job_limits(JobLimits::default());
    let (second, intents) = overflow();
    store.commit_with_jobs(second, intents).unwrap();
}

#[test]
fn exhausted_result_retention_still_records_terminal_state() {
    let (_directory, mut store) = open();
    let mut deployment = target();
    deployment.functions.get_mut("work").unwrap().arguments = Schema::String;
    store.retain_deployment(&deployment).unwrap();
    store.set_job_limits(JobLimits { bytes: 8 * 1024 * 1024, ..JobLimits::default() });
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

#[test]
fn expired_finished_jobs_free_scheduling_capacity() {
    let (_directory, mut store) = open();
    store.retain_deployment(&target()).unwrap();
    store.set_job_limits(JobLimits { jobs: 32, ..JobLimits::default() });
    let caller = json!({"player":"alice"});
    let mut expected = 1;
    for batch in 0..2 {
        let ids: Vec<_> = (0..16).map(|index| format!("job-{batch}-{index}")).collect();
        let scheduled = ids.iter().map(|id| JobIntent::Schedule(job(id))).collect();
        store.commit_with_jobs(commit(&format!("schedule-{batch}"), expected, vec![]), scheduled).unwrap();
        let cancelled = ids.into_iter().map(|id| JobIntent::Cancel { id, caller: caller.clone() }).collect();
        store.commit_with_jobs(commit(&format!("cancel-{batch}"), expected + 1, vec![]), cancelled).unwrap();
        expected += 2;
    }
    let late = || vec![JobIntent::Schedule(job("late"))];
    assert!(matches!(store.commit_with_jobs(commit("late", expected, vec![]), late()), Err(Error::JobBudget)));

    std::thread::sleep(std::time::Duration::from_millis(5));
    store.set_retention(crate::Retention { jobs: std::time::Duration::ZERO, ..crate::Retention::default() });
    store.commit_with_jobs(commit("late", expected, vec![]), late()).unwrap();
    assert_eq!(store.jobs().unwrap().records.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(), ["late"]);
}
