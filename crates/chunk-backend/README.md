# Environment backend

`Backend::new(Box<dyn Storage>)` starts one environment engine thread and one
commit thread. Supply a store with its schema installed and exclusive writer
authority. Register immutable bundles by `DeploymentId`, then use the async
`query`, `mutate`, and `subscribe` methods from transport tasks. Caller identity
must already be authenticated. This crate does not install schemas or provide
a network protocol.

The engine thread owns `chunk_js::Engine`, pinned storage snapshots, pending
writes and subscription dependencies. Each mutation executes against the latest
view, validates its writes against the snapshot's schema, and applies them to a
bounded overlay. Execution and validation are serialized, so another mutation
cannot change the read revision between them. The commit thread persists batches
in order while the engine can continue evaluating requests.

Mutation responses wait for durable commits. Queries may read staged writes,
but their responses wait until that revision is durable too. Subscriptions read
only acknowledged snapshots, track point misses and empty ranges, and reevaluate
after relevant commits. Dependencies refresh even when the JSON result is
unchanged. Result changes use JSON text equality; object key order can cause an
extra update. Slow subscribers coalesce updates through a watch channel, so they
receive the latest durable result rather than every intermediate revision.

Give every mutation a stable operation ID. Its fingerprint includes the bundle,
function, arguments and caller. Duplicate requests recover the stored outcome
without executing again; reuse with a different request fails. Dropping a request
future cancels queued/executing work, but a staged mutation still commits. Retry
with the same ID after a lost reply. Cancellation during a shared execution may
cancel duplicate waiters too; they can retry the same ID.

A commit or acknowledgment failure stops the pipeline, rejects further requests
and closes subscriptions. Later speculative batches are never persisted after a
failed batch. Restart with the same store and recover outcomes by operation ID;
the failed commit may already be durable. The backend does not automatically
retry an ambiguous speculative suffix.

Admission allows 64 outstanding requests, including replies waiting for
durability. Limits are 16 resident deployments, 64 subscriptions, 16 outstanding
mutations and 32 MiB of serialized pending writes/results. Excess work returns
`Error::Busy`. These are logical bounds, not an RSS limit. JS retains its own
source, heap, capability and payload budgets. Release a deployment after its
mutations and subscriptions drain.

The current storage API decodes documents into `serde_json::Value`; snapshot
reads run synchronously on the engine thread. Range scans can materialize an
entire interval before the JS payload budget rejects it. They need a bounded
storage read API before accepting arbitrary large-database scans. Commit results
are parsed on the commit thread because `chunk_store::Commit` currently takes
`Value`; query/subscription responses remain JSON text. Durable retries preserve
the JSON value but may normalize its formatting and object key order.

Construction waits for initialization. Dropping the last backend handle drains
accepted commits and joins both threads; use a blocking task for construction
and final drop from async code. Persistent module state is disposable: handlers
must derive transactional results from arguments, caller and tracked reads, as
described in `chunk-js`.

Focused checks: `cargo test -p chunk-backend -p chunk-store -p chunk-js` and
`cargo clippy -p chunk-backend -p chunk-store -p chunk-js --all-targets -- -D warnings`.
