# chunk-backend

The backend is the sync engine inside core. It runs a project's TypeScript queries, mutations, actions, hooks and
commands on embedded V8 ([`chunk-js`](../chunk-js/README.md)) over the environment's SQLite store
([`chunk-store`](../chunk-store/src/lib.rs)), keeps reactive subscriptions up to date, and runs scheduled jobs. Its only
ingress is core's `chunk.sync.v1` service in the [environment process](../chunk-environment/README.md), which
authenticates callers and supplies their identity; internal functions can't be called from there.

## Embedding

`server::run(Config { bundle, environment, state, replication }, ready, stop)` opens `<state>/environment.sqlite`
(replicated to object storage when `replication` is set), deploys the optional bundle, and hands back the `Backend`.
Once stopped, it flushes the replicated log and fails if that final flush does. To embed it directly,
`Backend::new(environment, Box<dyn Storage>)` takes a store with exclusive writer authority and starts one engine
thread, one commit thread, a job timer and one read engine per CPU (up to four). Construction waits for initialization,
and dropping the last handle drains accepted commits and joins the threads, so do both from a blocking task in async
code.

From transport tasks, use the async `query`, `mutate`, `subscribe` (one query) and `subscribe_group` (up to 16 queries
that update together). Actions, jobs and commands have their own methods below. Dropping a request cancels queued work
and running queries.

## Deployments and schema

`deploy` validates a `chunk_contract::Deployment` (the backend part of a release), installs its schema and indexes, and
retains its bundle before its functions become callable; retained bundles reload after a restart. It returns `Busy` if
commits are outstanding or another deployment change is in progress. At most 16 deployments are resident, and they all
serve against the one database.

Schema changes are additive. Table names are the keys of the schema the project's `server/schema/index.ts` exports, not
file paths. A deployment's tables are merged into the store's installed schema: new tables, optional new fields and new
indexes are accepted; changing an existing field or index is rejected. Tables starting with `chunk_` are reserved.

Release a deployment once its mutations and subscriptions have drained; `release` returns `Busy` while an action still
uses it. Releasing removes the bundle and permanently retires the ID; data, schema and operation outcomes stay. If a
retained bundle stops the backend from starting, open the store and call `Storage::release_deployment` with its ID.

## Mutations

Every mutation carries an operation ID. Its fingerprint covers the function, the canonical arguments and the caller, not
the deployment: a retry with the same ID returns the stored outcome without running again, even after a redeploy, and
reusing an ID for a different request fails. Outcomes are kept for 24 hours after their commit. Retry after a lost reply
with the same ID; a request whose outcome was never stored runs on the deployment it names, so transports must keep that
binding when retrying.

Mutations run one at a time against the latest state, including writes that are staged but not yet durable, and the
commit thread persists them in order. A reply waits until its commit is durable. Queries whose reads don't touch pending
writes answer at once from the durable state. If a commit is rejected (`Conflict`, `Invalid`, `Capacity`,
`OperationMismatch`, `RolledBack`, or a full job queue), the mutations staged after it fail with `Error::Retry`. An
ambiguous commit failure stops the pipeline and closes subscriptions; restart on the same store and recover outcomes by
operation ID. The backend never retries on its own.

## Subscriptions

Subscriptions read only durable snapshots. They record the documents, index ranges and misses they read, and rerun after
commits that touch them, in batches against one snapshot. Identical subscriptions share one evaluation until one reads
`ctx.caller`. Slow subscribers get the latest result rather than every revision. A group publishes once all its queries
hold for the same revision. Application errors are results too: they keep their dependencies and recover when the data
changes.

## Admission

Admission is bounded by waiting time and memory. New work is refused with `Error::Overloaded`, naming its `Limit`, once
queued work of the same kind has waited over 500 ms, or when its memory budget is spent:

| Budget                                   | Size                                                                                 |
| ---------------------------------------- | ------------------------------------------------------------------------------------ |
| Admitted requests and replies            | 64 MiB, each charged its input plus 1 KiB and any retained result                    |
| Pending mutations and their writes       | 32 MiB                                                                               |
| Subscriptions and their latest results   | 256 MiB                                                                              |
| Live actions                             | 32 MiB each, within an eighth of the machine's or cgroup's memory (at least 256 MiB) |
| Core's outgoing sync messages            | A sixteenth of that memory, at least 128 MiB (`Backend::send_budget`)                |
| Action and command records, prepared IDs | 64 MiB                                                                               |

Each function invocation may decode at most 4,096 rows or 4 MiB across all its reads, and `scanIndex` takes a limit of 1
to 1,024 results; exceeding a bound fails the read rather than truncating it. These are logical bounds, not an RSS
limit.

## Actions

Actions (`action` / `internalAction`) run outside transactions, each on a fresh isolate in a bounded worker. Their
context has `caller`, `invocationId`, `env`, `fetch`, `runQuery`, `runMutation` and `sleep`; it has no `db`. Each
nested query or mutation runs through the engine against a fresh snapshot, as the original caller and deployment, and a
mutation gets the operation ID `action/<invocationId>/<effect sequence>`.

Call `allocate_action_id` and then `start_action(id, call)`, keeping the ID when acceptance is uncertain: one ID starts
at most one invocation, and repeating the same request attaches to it. An action gets 30 seconds, 32 MiB of heap, 256
effects with at most eight pending, and 1 MiB inputs. Cancellation, failure or a lost backend can follow a committed
mutation; earlier effects are never rolled back, and actions are never retried automatically.

**Variables and secrets.** `ctx.env` holds the deployment's `chunk.toml` variables: its `[vars]`, with the
`[env.<name>.vars]` that `ActionEffects::with_vars` selects applied over them. Queries, mutations and hooks see those
alone. Actions and commands also see the environment's secrets, which `Backend::set_secrets` replaces at runtime: each
invocation keeps the secrets it started with, and later ones read the new set. Secrets live in memory only, are supplied
again after a restart, and are redacted from action logs and error messages that contain their values.

**HTTP.** `ctx.fetch({url, method, headers, body})` reaches any public HTTP(S) URL through one client shared by the
environment. It refuses loopback, private, CGNAT, link-local (cloud metadata included), unique-local, multicast and
reserved addresses after DNS resolution, and follows up to ten redirects itself, checking each; a redirect to another
origin drops `Authorization` and `Cookie`. Proxies, retries, decompression and cookies are off; bodies are UTF-8 text up
to 64 KiB out and 128 KiB back, with 32 headers of up to 8 KiB in total each way, and a fetch has ten seconds. Each
action has one fetch in flight. An outcome is `completed`, `rejected` (never sent) or `unknown` (sent, result lost), with
a stable `effectId`. Error messages never include URLs or bodies.

## Scheduled jobs

A mutation can call `ctx.scheduler.runAt(time, action, args)`, `cancel(jobId)` and
`retry(jobId, time, {acknowledgePossibleEffects: true})`; the job commits with the mutation's writes, under its caller.
The backend's timer starts due jobs, using up to a quarter of the live-action budget. A job is `pending`, `running`,
`succeeded` (with a result up to 64 KiB), `failed`, `cancelled` or `unknown`: a job interrupted by cancellation, a
deadline or a restart is `unknown`, is never restarted automatically, and needs an explicit `retry` that acknowledges
possible earlier effects. Its attempts run as `job/<jobId>/attempt/<n>`. The queue holds 20,000 jobs or 64 MiB
(`chunk_store::JobLimits`), with up to 16 intents per mutation and times at most 366 days ahead; a full queue fails the
whole mutation. Finished jobs are kept until `forget_job` or 24 hours after their last change.

The timer only runs while the process does. To wake a suspended host, read `Backend::wake_handoff()`, which holds the
earliest due time under a generation, install that alarm durably outside the process, and then call
`acknowledge_wake(generation, due_at)`; a changed generation rejects the acknowledgement, so read again. Core does this
with management's `SetWakeAlarm`.

## Commands and hooks

Core runs a player's commands and the project's hooks through the Rust API; the backend serves no transport of its own.
`command_catalog` lists the commands an app's scope declares and those the player may run, `command_suggestions` runs a
command's suggestion query, and `start_command` starts a command under an ID from `allocate_action_id`, rechecking
permission and parsing input against the declared command. Commands run like actions, with at most eight pending
platform effects (messages, player moves, session method calls), which the returned `CommandEffects` hands to core to
perform. A retry under the same ID joins the running command and never replays it.

## System tables

`Backend::system` returns a `System` for core's own state, such as [control](../chunk-control/README.md)'s.
`System::open` installs `chunk_`-prefixed tables and `System::commit` writes to them in one durable commit, ahead of
queued app commits. Apps can't read or write them. `System::lock_scope` holds a scope of system rows exclusively.

## Testing

`cargo test -p chunk-backend -p chunk-store -p chunk-js`. The `bench-support` feature exposes phase timings for
[`chunk-bench`](../chunk-bench/README.md).
