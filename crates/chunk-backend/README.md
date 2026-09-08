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
and responses that depend on those writes wait for durability. Queries whose
dependencies do not intersect pending writes return immediately at the base revision. Subscriptions read
only acknowledged snapshots, track point misses and empty ranges, and reevaluate
after relevant commits. Dependencies refresh even when the JSON result is
unchanged. Result changes use JSON text equality; object key order can cause an
extra update. Application errors remain reactive results, retaining reads collected
before failure; an error-to-success transition always publishes. Reevaluations run
one per actor scheduling boundary, with at most two retained durable snapshots.
Queued revisions may coalesce conservatively to the latest snapshot. Slow subscribers coalesce updates through a watch channel, so they
receive the latest durable result rather than every intermediate revision.

Give every mutation a stable operation ID. Its fingerprint includes the function,
canonical arguments and caller, independent of bundle and deployment identity.
Duplicate requests recover the stored outcome without executing again, including
after redeployment; reuse with a different request fails. Outcome lookups use the
engine's pinned base snapshot and pending operations, so execution need not wait
behind earlier commits. Inputs use `chunk_js::Json` (`Value::into()` or `Json::parse`)
to encode and canonicalize once before crossing the engine boundary.

Dropping a request cancels queued work and executing queries. Once a mutation
starts, an independent execution token prevents one caller from interrupting a
shared business operation. If every waiter has gone before staging, the mutation
is discarded; once staged, it commits. Retry the same ID after a lost reply.
A durable outcome is recovered across deployments, but a request without a stored
outcome executes the explicitly supplied deployment: transport must retain the
original deployment binding when retrying unresolved operations.

A deterministic commit rejection (`Conflict`, `Invalid`, `Capacity`, or
`OperationMismatch`) discards the speculative suffix and fails its waiters with
`Error::Retry`. Queries and subscriptions continue against the durable base;
new mutations return `Busy` until all old suffix acknowledgments drain, preventing
revision reuse from admitting an old dependent batch. Ambiguous commit or
acknowledgment failures stop the pipeline and close subscriptions. Restart with
the same store and recover outcomes by operation ID; a failed acknowledgment may
follow a durable commit. The backend never automatically retries a speculative suffix.

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
