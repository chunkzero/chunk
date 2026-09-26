# Environment backend

`Backend::new(environment, Box<dyn Storage>)` starts one environment engine thread, one commit thread and a local job
timer. Supply a store with exclusive writer authority. `deploy` validates and initializes a versioned
`chunk_contract::Deployment`, then atomically installs its additive schema/indexes and retains the bundle before
enabling public functions. Bundles and contracts reload after restart. Use async `query`, `mutate`, `subscribe`, or
`subscribe_group` from transport tasks. `Service` exposes authenticated gRPC; only trusted platform processes may supply
caller identity. Internal functions are inaccessible through this ingress. Activation waits for the commit pipeline to
drain; it returns `Busy` while work is outstanding. Queries can use existing deployments during activation. Schema
changes advance the revision; every successful activation reevaluates existing subscriptions.

The engine thread owns `chunk_js::Engine`, pinned storage snapshots, pending writes and subscription dependencies. Each
mutation executes against the latest view, validates its writes against the snapshot's schema, and applies them to a
bounded overlay. Execution and validation are serialized, so another mutation cannot change the read revision between
them. The commit thread persists batches in order while the engine can continue evaluating requests.

Mutation responses wait for durable commits. Queries may read staged writes, and responses that depend on those writes
wait for durability. Queries whose dependencies do not intersect pending writes return immediately at the base revision.
Subscriptions read only acknowledged snapshots, track point misses and empty ranges, and reevaluate after relevant
commits. Dependencies refresh even when the JSON result is unchanged. Result changes use JSON text equality; object key
order can cause an extra update. Application errors remain reactive results, retaining reads collected before failure;
an error-to-success transition always publishes. Reevaluations run one group (at most 16 queries) per actor scheduling
boundary, with at most two retained durable snapshots. Queued revisions may coalesce conservatively to the latest
snapshot. Slow subscribers coalesce updates through a watch channel, so they receive the latest durable result rather
than every intermediate revision. Each group evaluates all queries against one snapshot. Per-query failures occupy their
original result positions, retain dependencies, and recover reactively. Transport errors close the stream; clients mark
retained results stale until a fresh full group arrives on reconnect.

Give every mutation a stable operation ID. Its fingerprint includes the function, canonical arguments and caller,
independent of bundle and deployment identity. Duplicate requests recover the stored outcome without executing again,
including after redeployment; reuse with a different request fails. Outcomes are kept for 24 hours after their commit
(the default `chunk_store::Retention`); a retry after that executes again as a new operation. Outcome lookups use the
engine's pinned base snapshot and pending operations. New operations prepare their retry context on the commit thread
before execution. Inputs use `chunk_js::Json` (`Value::into()` or `Json::parse`) to encode and canonicalize once before
crossing the engine boundary. Argument/result contracts and wire numbers are validated before publication.
Deployment-specific reads project declared fields; writes must satisfy the physical schema and all resident contracts.
Storage's signed 64-bit support does not make arbitrary integers safe JavaScript values.

Dropping a request cancels queued work and executing queries. Once a mutation starts, an independent execution token
prevents one caller from interrupting a shared business operation. If every waiter has gone before staging, the mutation
is discarded; once staged, it commits. Retry the same ID after a lost reply. A durable outcome is recovered across
deployments, but a request without a stored outcome executes the explicitly supplied deployment: transport must retain
the original deployment binding when retrying unresolved operations.

A deterministic commit rejection (`Conflict`, `Invalid`, `Capacity`, or `OperationMismatch`) discards the speculative
suffix and fails its waiters with `Error::Retry`. Queries and subscriptions continue against the durable base; new
mutations return `Busy` until all old suffix acknowledgments drain, preventing revision reuse from admitting an old
dependent batch. Ambiguous commit or acknowledgment failures stop the pipeline and close subscriptions. Restart with the
same store and recover outcomes by operation ID; a failed acknowledgment may follow a durable commit. The backend never
automatically retries a speculative suffix.

Admission allows 64 outstanding requests, including replies waiting for durability. Limits are 16 resident deployments,
64 subscriptions, 16 outstanding mutations and 32 MiB of serialized pending writes/results. Excess work returns
`Error::Busy`. These are logical bounds, not an RSS limit. JS retains its own source, heap, capability and payload
budgets. Release a deployment after its mutations and subscriptions drain. Release durably removes the bundle and
permanently retires its identity before unloading the runtime. It cannot be reactivated under the same ID. Data and
schema remain shared; release never drops application tables or operation outcomes. Uncommitted operation IDs remain
bound to the retired deployment and return `OperationMismatch` if retried against another deployment. Clients must use
new operation IDs for those requests. Committed outcomes remain recoverable through a retained deployment exposing the
same mutation. If a retained bundle prevents startup, open the store with exclusive writer authority and call
`Storage::release_deployment` with its ID before constructing the backend again.

The storage API decodes documents into `serde_json::Value`; snapshot reads run synchronously on the engine thread. A
cumulative allowance limits each invocation to 4,096 decoded rows / 4 MiB, charging before field decoding. Exceeding it
fails the read instead of returning a silently truncated result. `scanIndex` supports declared ascending indexes,
equality prefixes and a half-open range on the next field, with 1–1,024 results. Both pending and invocation-local
writes participate in ordering and limiting. Dependencies include old/new index keys and empty ranges. Commit results
are parsed on the commit thread because `chunk_store::Commit` currently takes `Value`; query/subscription responses
remain JSON text. Durable retries preserve the JSON value but may normalize its formatting and object key order.

Construction waits for initialization. Dropping the last backend handle drains accepted commits and joins its owned
threads; use a blocking task for construction and final drop from async code. Persistent module state is disposable:
handlers must derive transactional results from arguments, caller and tracked reads, as described in `chunk-js`.

Focused checks: `cargo test -p chunk-backend -p chunk-store -p chunk-js` and
`cargo clippy -p chunk-backend -p chunk-store -p chunk-js --all-targets -- -D warnings`.

Mutation admission durably fixes the original snapshot timestamp, seed and uncommitted deployment binding before
evaluation. Definite rejection and restart preserve these inputs; committed retries still recover the original outcome
before execution and can cross deployment versions. New operations write their retry context in the commit thread's next
shared durable write, together with queued commits. Concurrent prepared operations still use the ordered speculative
pipeline. Retry contexts of operations that never committed are kept for 24 hours after preparation; an unresolved retry
after that executes with a fresh timestamp, seed and deployment binding.

## Bounded actions

The embedded SDK declares `action` / `internalAction` separately from transactions. Its `ActionContext` contains
`caller`, `invocationId`, `runQuery(reference, args)`, `runMutation(reference, args)`, and `sleep(milliseconds)`. It has
no `db` capability. Generated TypeScript exports `api` and `internal` references; internal functions remain unavailable
to public ingress. JVM transaction clients omit actions; platform code invokes them through the Rust backend API until
an action transport is provided.

Call `allocate_action_id()` before `start_action(id, call)` and keep that ID when acceptance is uncertain. Acceptance
starts at most one invocation per ID and retains its deployment. Repeating an identical request attaches to the same
scope and result; changing the caller, deployment, function or arguments fails. The returned `ActionHandle` exposes
status, cancellation, and an async outcome. Dropping its last clone cancels the scope. The backend retains 32 status
records and rejects retired IDs instead of executing them again; allocating IDs long before submitting them can result
in retirement. A new backend incarnation makes old IDs unknown. Actions are never automatically retried.

Actions run in separate bounded workers with fresh V8 isolates, so sleep, transaction waits and CPU work do not occupy
the foreground environment actor. Limits are eight live actions, 32 MiB managed heap and separately 32 MiB ArrayBuffer
backing storage per action, 30 seconds from acceptance (initialization also has the one-second module budget), 256
effects per invocation, eight pending effects per invocation, and 32 pending action transaction requests per
environment. Inputs, effect replies and results are each at most 1 MiB. Status records retain bounded request/result
payloads. These logical limits exclude V8/native overhead. Shutdown cancels workers and joins them after closing their
reply path.

Every nested query/mutation goes through the original environment's actor against a fresh snapshot. The host captures
the original caller and deployment; arguments cannot replace either authority. Internal references are permitted through
this trusted path. A mutation receives the durable operation ID `action/<invocationId>/<effect-sequence>`; each effect
has a distinct increasing sequence. No snapshot or transaction is held over a sleep. Deployment release returns `Busy`
while an action references it.

Cancellation, failure, backend loss or a missing reply can follow a committed mutation. They do not roll back earlier
effects. After process loss the action outcome is explicitly unknown, while completed mutations retain their ordinary
durable outcomes. Recover those outcomes under their derived operation IDs where necessary; do not restart the action
with a new ID to resolve uncertainty. Durable job scheduling is a separate layer; external effects require the host
grants described below.

### Scoped HTTP and secrets

`Backend::new` denies all external capabilities. An embedder can configure immutable grants with
`Backend::with_action_effects(environment, store, effects)`:

```rust,ignore
let grants = ActionGrants::default()
    .with_http("billing".into(), HttpBinding::new("https://billing.example/api/", [HttpMethod::Get, HttpMethod::Post])?)?
    .with_secret("billing-token".into(), std::env::var("BILLING_TOKEN")?)?;
let effects = ActionEffects::new(environment.clone())?
    .with_deployment(deployment_id, grants)?;
```

Grants apply only to the exact environment and deployment. They are held in host memory, never serialized into a bundle,
release or database. Restart requires the host to supply them again. At most 16 deployments can have grants; each has at
most 16 HTTP bindings and 16 secrets, each secret at most 8 KiB. The embedding host resolves environment variables or
its own secret source; JavaScript cannot enumerate environment variables or access arbitrary files.

Actions call `ctx.http("billing", {path: "invoices", method: "POST", body: "..."})` and
`await ctx.secret("billing-token")`. HTTP paths must be relative and remain under the configured base path and origin.
Path segments use unreserved ASCII characters; query parameters may use percent encoding. Absolute paths, userinfo,
fragments, traversal, matrix parameters and encoded path escapes are rejected. Bindings grant explicit methods.
Redirects, inherited proxies, automatic retries, automatic decompression and cookies are disabled. The API supports
UTF-8 text bodies, up to 64 KiB for requests and 128 KiB for responses, 32 headers / 8 KiB in each direction, 2 KiB
paths, and eight simultaneous HTTP requests across the environment. The binding timeout defaults to ten seconds and can
be lowered; the action's overall deadline also applies. Response bodies are read incrementally within their bound.

HTTP outcomes have `state: "completed" | "rejected" | "unknown"` and a stable `effectId` formed from the invocation ID
and effect sequence. `completed` contains `status`, `headers` and `body`; applications still need to interpret the HTTP
status. `rejected` means no dispatch occurred. Once dispatch begins, transport failure, response truncation, size limits
or timeout return `unknown`, because the remote operation may already have happened. No failed request is retried.

Cancellation or backend loss can terminate the action before JavaScript receives an HTTP outcome. Such a lost/cancelled
action leaves its dispatched effects uncertain; it does not guarantee delivery of an `unknown` result. Reusing the
action ID never restarts a retained or stale invocation. Reconcile with the remote service or an application idempotency
key before deciding to issue another business request. Action status is ephemeral and does not replace a durable job
record.

Automatic host HTTP errors contain no URL, body, headers or transport error text. Action diagnostics containing a
granted secret value or its JSON-escaped form are replaced with a generic redacted message. Secret reads intentionally
hand the value to authorized application code; transformed values and deliberate application publication are outside
literal redaction. Queries and mutations retain their pure capability profile. Deployment configuration, secret rotation
and hosted secret management remain separate platform work.

## Durable scheduled jobs

Mutations can atomically record `ctx.scheduler.runAt(unixMilliseconds, actionReference, args)`, `cancel(jobId)`, and
`retry(jobId, unixMilliseconds, {acknowledgePossibleEffects: true})` with their document writes and operation outcome.
`runAt` returns a stable job ID derived from the mutation operation and intent position. Arguments and action kind are
validated against the captured deployment before commit, including `internalAction` references. The server captures the
full originating caller; job arguments cannot select a caller, environment or deployment. Cancellation, retry,
`Backend::job` and `forget_job` require that same caller. Queries and actions cannot schedule directly; an action can
call a mutation to record intent.

`Backend` starts a local timer and dispatches due jobs automatically, checking at most every 100 milliseconds when the
actor is available. At most two scheduled jobs run within the existing eight-action limit. Busy action capacity leaves
jobs pending, or retains an already-durable claim until a worker is available. The commit thread durably changes
`pending` to `running` before any action starts. SQLite keeps job metadata and wake state in private tables, separate
from application schema. Other storage adapters must implement atomic scheduling; nonempty intents fail closed by
default.

A successful action records `succeeded` and a result up to 64 KiB; an oversized result or exhausted result retention
capacity records `failed` without that result. Observed application or contract errors also record `failed`; this does
not undo prior effects. Interrupted execution (cancellation, deadline, resource termination or backend loss) records
`unknown`, since nested mutations or external requests may already have effects. Startup turns every recovered `running`
attempt into `unknown` and never starts it again automatically. This also covers a crash between claim and dispatch.
`pending` jobs resume normally. Cancelling pending work prevents dispatch; cancelling running work records `unknown` and
expires its action scope. Effects already accepted elsewhere can still complete.

`invocationId` is `job/<jobId>/attempt/<number>`. Nested mutations use that prefix plus `/<effect-sequence>`; HTTP
outcomes use that prefix plus `/http/<effect-sequence>`. These identities survive restart and let trusted reconciliation
recover earlier mutation outcomes or correlate remote requests. They do not make an external service idempotent. A retry
is an explicit new attempt, allowed only for failed, unknown or cancelled jobs with acknowledgement of possible earlier
effects. Its captured caller, arguments and originating deployment remain fixed. Earlier attempt numbers remain usable
for reconciliation; the job record retains only the latest attempt's state and result.

Pending/running jobs retain their originating bundle across restart. Terminal records remain until their owner calls
`forget_job` or expire 24 hours after their last change; forgetting live work is rejected. Terminal records do not pin
code: releasing their deployment makes later retries fail. The queue retains at most 256 jobs / 8 MiB, with 16 intents
per mutation, 64 KiB per encoded intent and 64 KiB for captured caller data. `runAt` accepts a nonnegative safe integer
no more than 366 days beyond the mutation's captured time; times already due become immediately eligible. Expired
terminal records are removed before a new job is admitted, and queue overflow rejects the whole mutation. There is no
recurring schedule or automatic action retry. Host HTTP/secret grants must be supplied again after restart; grants and
secret values are never part of a job record unless application code explicitly puts such values in its
arguments/result.

### Host alarm handoff

The local timer runs only while the backend process runs. It cannot wake a suspended host. A hosting adapter reads
`Backend::wake_handoff()` to obtain durable `{generation, due_at, acknowledged, running}` state. Every scheduling change
advances the generation and recomputes the earliest pending due time atomically. The adapter must durably install or
clear its external alarm for that exact generation and time **before** calling `acknowledge_wake(generation, due_at)`. A
changed generation or time rejects the acknowledgement; read and reconcile again. `acknowledged` reports persisted
adapter handoff, not proof that a provider fired its alarm.

After resume/restart, construct the backend first so interrupted attempts recover and due work can dispatch, then read
and reconcile the latest handoff with the external alarm. Before suspension, the host must coordinate admission, drain
foreground requests and action scopes, wait for `running` to be zero, and reconcile until the latest generation is
acknowledged. Retain the external alarm independently of the suspended process. This API defines the durable handoff
contract; installing an alarm and resuming a machine are the hosting adapter's job.

## Platform commands

`CommandService::new(backend, application_credential, platform_credential)` exposes `BackendCommands` using the distinct
platform credential. The local backend server registers it alongside queries and hooks. Requests bind the environment
and deployment through metadata. Trusted proxy code supplies the captured player, connection, claim, session, app and
generation scope; the backend verifies the app's manifest membership/domain and reconstructs `caller`. Generation fields
in caller JSON are decimal strings. Command input cannot supply caller identity, an export or a different effect target.
The proxy verifies that the captured runtime assignment has arrived and remains eligible before dispatching effects.

`Catalog` returns inherited command descriptors, including denied roots, plus the currently allowed IDs. Keeping denied
roots in the ownership catalog prevents accidental forwarding to a native command with the same name. `Suggest` executes
only a suggestion query declared on the selected command. Permission and suggestion queries may be internal; neither
becomes callable through public query ingress. Authorization calls have a two-second aggregate deadline. Permission
checks use a durable view and fail with `Busy` while commits are outstanding.

`Prepare` validates permission and reparses input with the fixed descriptor, then allocates an invocation ID without
running its handler. `Run` begins with that ID and repeats permission/parser checks immediately before acceptance. Only
the first stream owns effects. Concurrent or later streams for that ID observe status and never replay the handler or
its effects. A lost `Prepare` reply is safe to prepare again because it has not executed code; after attempting `Run`,
keep the same ID. Missing/expired IDs mean an unknown outcome, never permission to retry the command under a new ID.
Prepared identities expire after 60 seconds; at most 64 prepared/retained entries and 16 streams are kept. Capacity can
evict unstarted or terminal entries earlier. Process restart loses this ephemeral history. Preparation alone does not
retain a deployment; accepted execution retains it until its worker exits.

Commands use the existing action workers, deployment HTTP/secret grants, 30-second deadline, eight live actions and 256
total effects. Each platform stream has at most eight pending effects. The backend checks the original command's
permission again before every nested transaction and platform effect. Functions and hooks have no platform capability;
hooks also retain their read-only rules and default denial of HTTP/secrets. `followPlayer` changes eligible proxy effect
delivery only; it never changes the backend's original caller, permission, deployment or domain binding. Session calls
remain bound to the original app/session and declared session-method argument/result schemas.

Platform requests/replies are at most 64 KiB. Text is at most 4,096 UTF-16 units; suggestions contain at most 64 strings
of 256 units each. Effects have stable IDs `action/<invocationId>/platform/<effect-sequence>`. Text, routing and session
send receipts contain `{state: "accepted", operationId}`; this acknowledges downstream acceptance, not completion of
player delivery or gameplay work. Session calls return the declared result. No handler or external effect is retried
automatically. Cancellation, owner disconnect, deadline or process loss can leave earlier mutations and dispatched
effects completed even when the command outcome is unknown. Disconnect and shutdown cancel the action, resolve pending
effect waits and close the stream. A duplicate observer disconnect only detaches; its explicit cancel cancels the owner.
