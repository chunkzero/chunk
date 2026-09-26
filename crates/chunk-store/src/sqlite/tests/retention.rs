use std::time::Duration;

use super::*;
use crate::Retention;

const EXPIRED: Retention = Retention { outcomes: Duration::ZERO, retry_contexts: Duration::ZERO, jobs: Duration::ZERO };

fn contexts(store: &SqliteStore) -> usize {
    store.connection.query_row("SELECT count(*) FROM _chunk_retry_contexts", [], |row| row.get(0)).unwrap()
}

#[test]
fn outcomes_and_retry_contexts_expire_only_after_their_window() {
    let (_directory, mut store) = open();
    store.commit(commit("old", 1, vec![])).unwrap();
    let context = RetryContext { deployment: "v1".into(), timestamp: 1, seed: 2 };
    store.prepare_operation(&operation("abandoned"), context).unwrap();
    store.pruned_at = None;
    store.commit(commit("inside", 2, vec![])).unwrap();
    // A retry inside the window still finds its outcome instead of running again.
    assert_eq!(store.commit(commit("old", 0, vec![])).unwrap().revision, Revision(2));
    assert_eq!(contexts(&store), 1);

    std::thread::sleep(Duration::from_millis(5));
    store.set_retention(EXPIRED);
    store.pruned_at = None;
    store.commit(commit("new", 3, vec![])).unwrap();
    assert!(store.outcome(&operation("old")).unwrap().is_none());
    assert!(store.outcome(&operation("inside")).unwrap().is_none());
    assert!(store.outcome(&operation("new")).unwrap().is_some());
    assert_eq!(contexts(&store), 0);
}
