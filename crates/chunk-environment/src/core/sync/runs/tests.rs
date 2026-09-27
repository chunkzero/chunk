use super::*;

const OPERATION: &str = "prep:1";

/// A backend to charge subscriptions against, with the directory its store lives in.
fn backend() -> (Backend, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = chunk_store::SqliteStore::open(directory.path().join("environment.sqlite"), "test").unwrap();
    (Backend::new("test".into(), Box::new(store)).unwrap(), directory)
}

/// Follows the command under [`OPERATION`] for gateway credential `gateway`, reserving it if unused.
fn follow(runs: &Arc<Runs>, backend: &Backend) -> Subscription {
    let (stream, charge) = (CancellationToken::new(), backend.charge_request(RECORD).unwrap());
    runs.follow(OPERATION, None, "gateway", &stream, CancellationToken::new(), charge).unwrap().1
}

/// Starts `say` with `input` under [`OPERATION`] as `chunk:command` does, returning its run and whether this start
/// admits it.
fn begin(runs: &Runs, input: &str) -> Result<(Arc<Run>, bool), Error> {
    let request = CommandRequest::new("say", input, "player");
    let (run, new) = runs.begin(OPERATION, "gateway", request, CancellationToken::new())?;
    run.permits("gateway", Some(request))?;
    Ok((run, new))
}

#[tokio::test]
async fn a_closed_subscription_cancels_its_command_even_once_another_follows() {
    let (runs, (backend, _directory)) = (Arc::new(Runs::default()), backend());
    let (run, _) = begin(&runs, "say wait").unwrap();
    run.start().unwrap();
    let first = follow(&runs, &backend);
    drop(first);
    let _second = follow(&runs, &backend);
    assert!(run.token().is_cancelled());
}

#[tokio::test]
async fn a_reservation_closing_just_after_its_command_started_cancels_it() {
    let (runs, (backend, _directory)) = (Arc::new(Runs::default()), backend());
    let reservation = follow(&runs, &backend);
    let (run, _) = begin(&runs, "say wait").unwrap();
    run.start().unwrap();
    drop(reservation);
    assert!(run.token().is_cancelled());
}

#[tokio::test]
async fn a_reservation_closing_once_its_command_was_admitted_refuses_its_start_until_forgotten() {
    let (runs, (backend, _directory)) = (Arc::new(Runs::default()), backend());
    let reservation = follow(&runs, &backend);
    let (run, _) = begin(&runs, "say wait").unwrap();
    drop(reservation);
    assert_eq!(run.start().unwrap_err().code(), Code::Stopped);
    run.finish(outcome(&backend, Ok("null".into())));
    runs.settle(OPERATION, &run);
    assert_eq!(begin(&runs, "say wait").err().map(|error| error.code()), Some(Code::Stopped));
}

#[tokio::test]
async fn a_reservation_keeps_its_first_request_once_rejected_while_a_duplicate_waits() {
    let (runs, (backend, _directory)) = (Arc::new(Runs::default()), backend());
    let _reservation = follow(&runs, &backend);
    let (run, _) = begin(&runs, "say a").unwrap();
    let (duplicate, new) = begin(&runs, "say a").unwrap();
    assert!(!new);
    let duplicate = tokio::spawn(async move { duplicate.started().await });
    tokio::task::yield_now().await;
    runs.reject(OPERATION, &run);
    assert_eq!(begin(&runs, "say b").err().map(|error| error.code()), Some(Code::OperationMismatch));
    assert!(!duplicate.await.unwrap().unwrap());
    assert!(begin(&runs, "say a").unwrap().1);
}
