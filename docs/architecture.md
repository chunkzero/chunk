# Repository architecture

This page maps the v1 direction to this repository. The
[knowledgebase](https://github.com/chunkzero/knowledgebase) owns the platform
design and unresolved decisions. Except for the protocol and proxy path,
the components below are scaffolds, not functioning services or public APIs.

## Ownership

A project contains apps and has environments such as `prod` and `beta`. Each environment
owns its database and one authoritative backend with an embedded JS runtime.
An immutable deployment identifies functions, schema, server JAR and asset
references. The backend retains multiple deployment versions while sessions,
subscriptions and scheduled jobs still reference them. Nested function calls
retain the originating deployment. There is no separate function-runner pool.

Chunk automatically provisions sessions from server-owned routing/demand policies.
App metadata supplies optional machine-profile requirements; queues/matchmaking are
optional server libraries, not built into the app or Gradle model. A gameplay JVM belongs to one environment, deployment
and profile, and hosts multiple sessions. Session lifecycles must scope worlds,
players, tasks and cleanup independently. Application code does not provision
machines or manually create platform sessions. Exact demand and admission
policies remain open.

The control plane owns placement, machine provisioning and rollouts. A local
runtime supervises gameplay JVMs and reports health. New capacity can be loaded
and optionally suspended before use. Retirement requests graceful completion;
maximum machine age initiates draining followed by a configurable shutdown
deadline. New admissions prefer the promoted deployment; eligible reconnects may
return to old sessions. Detailed grace and forced-deadline behavior remain open.

The proxy owns Minecraft connections, authentication and routing. Backend
execution is distinct from the global proxy fleet. The current CLI's `edge`
name does not require these services to share a process. Planned proxy updates
and application rollouts are separate drain operations; crash recovery does
not promise preservation of a lost JVM's live simulation.

## Crates

Existing crate names are retained until implementation gives us reason to
change their boundaries. This is a responsibility map, not a dependency policy.

| Crate | Current or intended responsibility |
| --- | --- |
| `chunk-protocol-derive`, `chunk-protocol-codegen`, `chunk-protocol` | Implemented Minecraft codecs and packet generation; no sockets |
| `chunk-proxy` | Implemented login, configuration and waiting world; future routing and player moves |
| `chunk-edge` | Currently a proxy entry point; proposed home for sync-engine orchestration in the environment backend |
| `chunk-js` | Initial `deno_core`/V8 embedding; capability interface and implementation open |
| `chunk-store` | Separate persistence abstraction; SQLite first, then hosted Turso and self-hosted Postgres/MySQL |
| `chunk-contract` | Deployment manifests, function/schema descriptions and declarative session requirements |
| `chunk-control` | Automatic placement, host provisioning, rollout reconciliation and directory |
| `chunk-runtime` | Local JVM supervision and reporting |
| `chunk-build` | Toolchain orchestration, bundling, generated clients, JAR builds and asset publication |
| `chunk-proto` | Intended Rust transport bindings; generation not implemented |
| `chunk` | CLI; currently only the proxy `edge` command is functional |

The sync engine controls JS execution, tracks reads and writes, validates and
retries transactions, and maintains reactive subscriptions. Storage must support
consistent reads, atomic durable commits and recovery. The exact split for
snapshots, conflict validation, revisions and change records is open; do not
reduce the adapter contract to CRUD. An ambiguous commit needs a recoverable
outcome, not a blind retry. One authoritative backend avoids coordinating
independent writers, but does not by itself supply these guarantees.

Schema evolution is intended to use additive changes and backfills compatible
with retained deployments. Versioned assets and framework world templates are
immutable. Gameplay may modify worlds in memory. There are no framework-managed
mutable world saves; general object storage may support application-managed
exports later.

## JVM and JavaScript

`jvm/runtime` is the current runtime scaffold. Chunk owns both the public JVM
SDK and Minestom integration, including multiple sessions per process and
generated backend clients. The module split between public API and runtime
implementation remains open; no separate API module exists yet.
`jvm/proto` is the shared transport binding scaffold. `jvm/gradle-plugin` is
currently a no-op plugin.

`packages/server` is the empty `@chunk/server` SDK scaffold; it has no exported API.
V1 targets minimal JavaScript plus explicit engine capabilities and pure-JS
libraries, without ambient Node or browser globals. Start with `deno_core`/V8. Bundling
will likely use Rolldown; type checking, contract extraction and exact client
APIs remain open. Runtime restrictions do not dictate which tools may be used
during builds.

## Developer layout and execution contracts

Each `apps/<id>/` owns `app.toml`, `build.gradle.kts`, JVM source, assets, and optional
backend source. Root settings registers Gradle modules; a root build may apply shared
plugins. The app ID comes from its directory. TOML selects a command domain and optional
runtime profile. Gradle handles build dependencies/conventions, not gameplay policy.
Typed gameplay config lives beside owning source. No `app.ts` is required.

`chunk gen` (proposed) generates backend/config types before JVM compilation. The build
then validates annotated session implementations; annotation names and parameter-contract
extraction remain open. Schemas compose explicitly at `server/schema/index.ts`, keeping
table identity independent of file paths. Project deployments include all app modules.

Static `server/domains/` folders define inherited proxy command/hook scopes. Domain
commands execute in the backend via gRPC, with per-player command manifests pinned to
their deployment. JVMs supply app-local commands. Named `createHook` exports select typed
event contracts: some await admission/routing results, others notify after transitions.
Proxy loops remain nonblocking. Root/ancestor scopes persist when a player moves between
sibling domains; network connect/disconnect differs from domain enter/leave.

Async commands receive acceptance before completion and can issue typed proxy effects,
host-managed delays, or calls/notifications to captured session references. Following a
player across moves is explicit opt-in, retains original code/version, and requires
current authority. Ephemeral work is distinct from durable jobs; retry, cancellation,
permissions, and bounded delivery remain implementation work.

The distributed proxy directory tracks project/environment membership separately from
command domains and gameplay data. Proposed control-plane gRPC snapshots/watches share
ownership, presence, health, and transfer state. Redis/Upstash is a possible cache or
distribution layer, not a selected exclusive-ownership authority. The knowledgebase owns
the detailed consistency/fencing requirements.

## Hosted and self-hosted

Shared code owns sync, session and rollout logic, assets, SDKs and the toolchain.
Hosted adapters target Fly.io, a global Rust proxy fleet and one Turso database
per environment, with billing, usage accounting and hosted UI.

Hosted backends wake on server-list pings and must be ready before gameplay
admission. Wake coalescing, scanner filtering and idle rules need implementation;
scheduling and external work also need an explicit idle policy. Configuration-
screen waiting is preferred for startup, but current code uses a waiting world.

App TOML can override the default named machine profile. The initial hosted `small`
proposal is 2 shared vCPUs / 512 MiB; larger sizes and admission limits are open.
Machine memory and session capacity are distinct. Owners choose requirements;
chunk selects compatible capacity and provisions it automatically.

Self-hosting targets always-on services with Docker, Podman or Apple containers,
SQLite/Postgres/MySQL adapters, the bundled proxy and a basic dashboard. BYO
proxy is outside v1. Dashboard/management scaffolding exists on the separate
`feat/self-hosted-dashboard-assets` branch; this checkout does not include it.
Asset publication remains proposed.

## Transport status

`proto/chunk/v1` is an incomplete proposal, not a supported or generated
transport. It sketches control commands, calls and packet streams. Internal
create/end commands are control-plane instructions, not application APIs.

Environment and deployment scope must accompany version-sensitive operations.
Transport authentication must bind these identities rather than trusting a
caller-supplied identifier. The scaffold does not yet specify ownership fencing,
reconnect/resumption, deadlines, commit-outcome recovery or atomic subscription
updates. Those must be resolved before implementation.

Player frames are proposed as packet ID plus payload, without outer framing,
compression or encryption; the proxy handles client transport. Connection count,
dial direction and relay topology are not fixed. Separate connections may carry
control, function calls and player traffic.
