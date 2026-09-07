# Environment backend

`Backend` exclusively owns an environment `Storage`, loads immutable
`chunk_contract::Deployment` bundles, and runs their explicit query/mutation
exports through `chunk-js`. `Service::into_server()` exposes the generated gRPC
service. Bind it to loopback; credentials and caller context belong to trusted
platform processes, not Minecraft clients.

Handlers receive `(ctx, arguments)`. `ctx.db.get(table, id)` returns a document
or null; `scan(table, start, end)` returns ordered `{id, value}` rows over the
inclusive-start, exclusive-end primary-key index. Mutations additionally have
`put(table, id, value)` and `delete(table, id)`. Undeclared tables are rejected.
Argument, result and document validators preserve optional versus nullable
object fields and reject unknown fields. Secondary field indexes and external
actions are outside this local implementation.

Mutations validate point/range dependencies and all retained document contracts
before one durable commit. Reuse the identical request and operation ID after
a timeout or lost response. Reserved `__chunk` identities belong to backend
metadata. Deployment hashes and table contracts persist in that metadata;
the runner must reload matching source artifacts after restart. Persisted
contracts remain enforced even before their source is reloaded.

Subscriptions publish complete groups from a single snapshot and coalesce
commits for slow consumers. A reconnect starts a fresh group. The JVM
`backend-client` pins its deployment and signals stale retained values until a
fresh snapshot arrives; close subscriptions with their session scope.

Local limits are four executing isolates, eight subscriptions, sixteen queries
per group, sixteen retained deployments, eight mutation attempts, and 1 MiB per
request/group result. Each isolate keeps the execution/heap limits of `chunk-js`;
SQLite retains its bounded materialized-snapshot capacity. These limits bound
components, not total process RSS. No deployment retirement or outcome expiry is
implemented yet.
