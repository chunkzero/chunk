# Repository architecture

This page maps the implemented local platform to its repository boundaries. The
[knowledgebase](https://github.com/chunkzero/knowledgebase) owns the broader platform design and unresolved decisions.
Proposed domains, hosted APIs and rollout features below remain separate from the implemented build and runtime.

## Ownership

A project contains apps and has environments such as `prod` and `beta`. Each environment owns its database and one
authoritative backend with an embedded JavaScript runtime. An immutable release identifies backend functions, schema,
self-contained app JARs and assets. The backend can retain multiple deployments against the same database. Session
clients pin their deployment and caller identity across calls and watch reconnects. The local development runner runs
one release at a time; overlapping deployment rollouts remain deferred.

Chunk provisions sessions from server-owned admission and routing policies. App metadata supplies capacity and optional
machine-profile requirements; queues and matchmaking remain server-owned policy. A gameplay JVM belongs to one
environment, deployment, app and profile, and can host multiple sessions. Each app declares session factories, while
each session owns its instances, players, tasks, subscriptions and cleanup independently. App code requests session
completion through its scope; control owns placement and process creation.

Local control persists reservations, activation intent and ownership generations. Control hosts supervise app-owned JVMs
directly, report health and reconcile authenticated process inventory. Player moves withdraw the old delivery before
activating its replacement. Drain stops new placement and moves players before shutdown; unresolved ownership is
retained until cleanup is confirmed. Hosted capacity, suspension, promotion and rollout reconciliation remain future
work.

The proxy owns public Minecraft connections, online authentication, encryption, compression and routing. Players can
move between sessions and JVMs on the same public connection. The development runner embeds backend, control and edge
services, while standalone binaries host the same libraries independently. An unexpected service exit stops the
development stack. A lost gameplay JVM loses its live simulation; packet replay does not restore its worlds.

## Crates

This is a responsibility map, not a dependency policy.

| Crate                                                               | Implemented responsibility                                                                                |
| ------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `chunk-protocol-derive`, `chunk-protocol-codegen`, `chunk-protocol` | Minecraft codecs and packet generation without sockets                                                    |
| `chunk-proxy`                                                       | Login, authentication, configuration, routing, player moves and waiting world                             |
| `chunk-edge`                                                        | Hosting the proxy with backend admission/routing and local control                                        |
| `chunk-js`                                                          | Bounded `deno_core`/V8 execution with explicit transactional capabilities                                 |
| `chunk-store`                                                       | SQLite snapshots, indexes, durable commits and operation-outcome recovery                                 |
| `chunk-backend`                                                     | Environment sync engine, retained deployments, transactions, subscriptions and authenticated backend RPCs |
| `chunk-contract`                                                    | Validated deployment, function, document and schema contracts                                             |
| `chunk-control`                                                     | Durable local placement, reservations, ownership and process reconciliation                               |
| `chunk-build`                                                       | App inventory, TypeScript checking/bundling, client generation and portable release publication           |
| `chunk-proto`                                                       | Generated Rust protobuf and gRPC bindings                                                                 |
| `chunk-service`                                                     | Service lifecycle, cancellation and logging helpers                                                       |
| `chunk-cli`                                                         | Project inspection, generation, build orchestration, local development and player operations              |

The backend engine thread owns JavaScript evaluation, snapshots, read dependencies and staged writes. Its commit thread
persists ordered batches. Mutation replies wait for durable commits; stable operation IDs recover outcomes after an
uncertain reply. The backend does not automatically retry failed mutations. Subscriptions evaluate complete query groups
against acknowledged snapshots, including reactive query errors. Clients retain the last result as stale during
transport interruption and replace it with a fresh group on reconnect. See the
[backend](../crates/chunk-backend/README.md) for durability and admission limits.

Schema activation validates additive changes against retained deployments. Table identity comes from explicit schema
composition, independently of file paths. Gameplay can modify worlds in memory, but framework-managed mutable world
saves and application object-storage exports remain deferred. SQLite is implemented; hosted Turso and alternative
self-hosted storage adapters are proposals.

## JVM and JavaScript

The JVM has a Java core and optional Kotlin adapters:

| Module                                                     | Responsibility                                                              |
| ---------------------------------------------------------- | --------------------------------------------------------------------------- |
| `jvm/backend-api`                                          | Java codecs, typed references and document identifiers                      |
| `jvm/backend-client`                                       | Asynchronous Java calls, mutation operation IDs and watch state             |
| `jvm/runtime`                                              | Generic Java 25 process lifecycle, deployment binding, readiness and health |
| `jvm/runtime-minestom`                                     | Minestom sessions, scoped resources, player admission and tick scheduling   |
| `jvm/backend-client-kotlin`, `jvm/runtime-minestom-kotlin` | Owned coroutine scopes, suspending hooks and Flow adapters                  |
| `jvm/proto`                                                | Generated Java protobuf and asynchronous gRPC bindings                      |
| `jvm/gradle-plugin`                                        | App discovery, explicit JVM toolchains, generation and artifact descriptors |

Java apps require no Kotlin production dependencies. Generated Java records, sealed unions and typed references are
shared by both languages. The optional `CoroutineBackendClient` facade uses those same Java models and an owned
coroutine adapter. Session hooks and continuations that modify gameplay run through the process tick executor; session
and player resources have separate lifetimes. See the [runtime](../jvm/runtime/README.md) and
[backend client](../jvm/backend-client/README.md) APIs.

`crates/chunk-build/sdk/` contains the TypeScript SDK embedded in the CLI. Projects import schema-bound builders and
types through `#chunk`, with independent schema helpers at `#chunk/schema`. The compiler checks TypeScript with the
pinned native compiler, bundles with Rolldown and extracts contracts by evaluating declarations in the bounded
JavaScript engine. The SDK is embedded in the CLI; consumers do not install a Node toolchain through Gradle. Runtime
code has language APIs and a bounded web subset, without ambient filesystem, network, process or Node capabilities. See
[JavaScript execution](../crates/chunk-js/README.md) for its limits.

## Developer layout and execution contracts

The root `chunk.toml` and immediate `apps/<id>/app.toml` files define the project. Each app owns `build.gradle.kts`, JVM
source and optional backend source/assets. Its directory supplies its ID. TOML contains capacity and machine-profile
requirements; Java toolchains remain explicit in Gradle. No second app list, command domain or `app.ts` is required.
Shared gameplay can be an ordinary Gradle project, as in `examples/local/shared`.

The settings plugin calls `chunk inspect` and includes the discovered app projects without generating code.
`chunk build PROJECT` invokes the project's wrapper with `chunkArtifacts`. That task graph runs `chunk gen` before JVM
compilation and emits one shared Java bindings JAR, plus a separate Kotlin facade JAR when requested. The descriptor
lists each app executable with its own dependencies. An installed or configured CLI supplies the compiler; consumer
plugins do not build Rust tools or install Node packages.

Each app JAR contains a local Java service registry generated from annotated `@SessionType` factories. Each public
provider implements `Session create()` and creates fresh state for each session. The app owns its main function and
includes the runtime libraries in its executable JAR. Gradle exports session type IDs separately for release assembly;
provider class names remain internal to the JVM. The external release manifest binds artifacts to resolved deployment
requirements from `[runtime]` and `[sessions.<id>]` in app TOML. Control selects the app and machine profile, verifies
the artifact digest, and supplies launch identity and session commands with admission limits. Registration authenticates
that launch identity. The JVM loads no deployment manifest. JVMs contain one app, and separate machine profiles require
separate JVMs. Dependency injection remains deferred.

The CLI publishes `dist/<id>/` and a matching `dist/<id>.tar.gz` containing backend code/contracts, normalized release
metadata, content-named JARs and explicit assets. One digest identifies the release and backend deployment. Generated
sources, SDK caches and machine-local descriptor paths are excluded. Publication validates app identities,
dependency/class conflicts and Java requirements; existing immutable releases are verified before reuse. See the
[compiler and release documentation](../crates/chunk-build/README.md).

`chunk dev PROJECT` builds the same release, uses the Gradle-selected Java executable unless overridden, and stores
local state under `PROJECT/.chunk/local`. The project's `[local]` settings and resolved app requirements configure
placement. Root framework Gradle workflows and the standalone app build are separate. The consumer acceptance check
exercises both Java and Kotlin projects from source copies without starting gameplay.

Scopes, hooks and commands are declared only in `apps/**/scope.ts` and `app.ts`; see the
[SDK documentation](../crates/chunk-build/sdk/README.md). Cross-domain event semantics, typed proxy effects and durable
job APIs remain proposed. The existing session hooks and scoped Minestom events remain the implemented gameplay APIs.
Distributed proxy-directory replication and cross-proxy transfers are also deferred; Redis/Upstash has not been
selected as an ownership authority.

## Hosted and self-hosted

Shared libraries implement local sync, sessions, SDKs and the toolchain. Hosted adapters, management/authentication
APIs, artifact upload and deployment promotion are deferred. The broader hosted design targets Fly.io, a global Rust
proxy fleet and one Turso database per environment, with billing, usage accounting and a UI.

Hosted wake-on-ping, wake coalescing, scanner filtering and idle policies still need implementation. Scheduling and
external work also need an idle policy. The current proxy uses a waiting world during startup; a configuration-screen
waiting flow remains a separate design choice.

App TOML can override the default named machine profile. The initial hosted `small` proposal is 2 shared vCPUs / 512
MiB; larger sizes and hosted admission limits remain open. Machine memory and session capacity are distinct. Local
profiles already specify memory and session limits for plain Java processes.

Self-hosting currently uses local Java processes and SQLite. Docker, Podman or Apple container adapters, Postgres/MySQL
storage and a management dashboard remain proposals. BYO proxy is outside v1. Dashboard/management scaffolding exists on
the separate `feat/self-hosted-dashboard-assets` branch and is not part of this checkout.

## Transport status

`proto/chunk/v1` generates matching Rust and Java bindings. `Backend`, `LocalControl`, `Gameplay`, `Supervisor`,
`NodeControl`, `ProcessControl` and `Players` have implementations. These internal RPCs cover backend calls/watches,
placement, process/session lifecycle, delivery preparation and player operations. The remaining `Directory`, `Runtime`
and `EdgeCall` services remain proposals. Application code uses session and typed backend APIs instead of issuing
provisioning commands directly.

Service credentials authenticate trusted platform processes. Deployment, process/session generations and player
ownership accompany version-sensitive operations and are checked against registered state. Mutation outcome recovery,
watch staleness and complete subscription groups are implemented. Reconciliation retains unresolved ownership rather
than treating a missing reply as cleanup.

Player traffic uses dedicated native Minecraft TCP connections directly to Minestom. Single-use delivery capabilities
pass through login plugin messages; configuration and play then use the normal Minecraft protocol. gRPC carries control
and backend traffic, while the proxy owns the public connection's authentication, encryption and compression.
