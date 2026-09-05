# Architecture

How this repository is cut: processes, crates, modules, and the lines
between them. The platform design (what the edge API looks like, what a
session is, why there are two tiers) is out of scope for this page.
Everything here is scaffolded and **proposed** until a crate says otherwise.

## Processes

One binary, `chunk`, in four roles:

| role      | command                 | hosts                                                                     |
| --------- | ----------------------- | ------------------------------------------------------------------------- |
| edge      | `chunk edge`            | player connections, app runtimes, databases, dashboard and management API |
| runtime   | `chunk runtime`         | session processes on a host; terminates their connection                  |
| control   | `chunk control`         | directory, placement, provisioning, cookie keys                           |
| toolchain | `chunk dev` and friends | the build pipeline, codegen, dev server                                   |

`chunk run` and `chunk dev` host edge, runtime and control in one process.
Self-hosting runs the three as separate processes on one box or many.

```
 players ──▶ edge ─────────────────────────────┐
             │ proxy · js · store · primitives  │ Directory
             │ EdgeCall (served)                ▼
             │                               control
             │ Players frames, EdgeCall relay   ▲
             ▼                                  │ Directory
           runtime ─── Runtime.Attach ───▶ session process (JVM)
             │ JVM supervision; host provisioning belongs to control
```

## The one connection rule

A session process **dials out once** and never listens. chunk starts it with
an address and a token; it opens `Runtime.Attach` and keeps it for life.
Everything else flows over that HTTP/2 connection:

- commands down the control stream, each answered by id: create session, end
  session, call an `@Expose` method, deliver a player, withdraw a player,
  prepare for restart, stop
- events up: register with session types and registry data, session ended,
  player left, health, diagnostics
- one `Players.Stream` per delivered player, opened by the process when it
  receives `Deliver`, carrying plain Minecraft frames both ways
- `EdgeCall.Invoke` and `Subscribe`, which the generated `Edge` client uses

Whoever answers that address implements all three services. In `chunk run`
that is the single process; in production it is `chunk runtime`, which relays
player frames and edge calls to the edge. This works behind any host boundary
with outbound connectivity only, and it means the JVM never has a listening
socket of any kind. `Runtime`, `Report` and `Players.Deliver`/`Withdraw`
could be separate services; here they collapse into the control stream
because the process cannot be a server.

## Crates

Layered bottom to top. A crate may depend only on crates in rows above it.

| crate                    | responsibility                                                  | depends on                                |
| ------------------------ | --------------------------------------------------------------- | ----------------------------------------- |
| `chunk-contract`         | contract IR and manifest as data                                | nothing                                   |
| `chunk-proto`            | Rust bindings for `proto/`                                      | nothing                                   |
| `chunk-protocol-derive`  | wire codec and packet derives                                   | nothing                                   |
| `chunk-protocol-codegen` | packet generation from pinned datasets                          | nothing                                   |
| `chunk-protocol`         | Minecraft wire protocol, no sockets                             | protocol-derive, protocol-codegen         |
| `chunk-management`       | static dashboard and authenticated management HTTP              | nothing                                   |
| `chunk-store`            | per-app SQLite, single writer, subscriptions, durable jobs      | contract                                  |
| `chunk-js`               | QuickJS executor behind an engine-independent interface         | contract                                  |
| `chunk-proxy`            | connection ownership: login, configuration, relay, park, move   | protocol, proto                           |
| `chunk-edge`             | app hosting: functions, events, primitives, `EdgeCall`, packs   | contract, proto, store, js, proxy         |
| `chunk-runtime`          | local JVM supervision and reconciliation client                 | contract, proto                           |
| `chunk-control`          | directory, placement, reconciliation, host provisioning         | contract, proto                           |
| `chunk-build`            | edge compiler, codegen, manifest                                | contract, js                              |
| `chunk`                  | the binary: CLI and the three long-running roles                | edge, runtime, control, build, management |

Rules the layering encodes:

- `chunk-protocol` decodes bytes and has no opinion. `chunk-proxy` has the
  connection policy. The edge currently supplies MOTD and login rejection text
  through `chunk_proxy::Config`; app-driven decisions are not implemented.
  Neither crate knows JavaScript exists.
- `chunk-js` and `chunk-store` never meet directly. `chunk-edge` installs
  store-backed capabilities on `ctx`, so the database outlives any runtime.
- Nothing in Rust ever sees a Minestom type. The manifest and contract are the
  whole of what chunk knows about an app.
- `chunk-runtime` knows what a session process is. The controller owns remote
  host provisioning. Local/container adapters belong in `chunk-control`;
  Fly integration belongs in a separate private platform. Suspend/resume is
  an optional backend capability.
- `chunk-build` uses the same executor as production for the capability-free
  compiler pass, so "declarations are pure" is checked by the real engine.

## JVM modules

| module                | artifact              | responsibility                                                     |
| --------------------- | --------------------- | ------------------------------------------------------------------ |
| `jvm/proto`           | `chunk-proto`         | Kotlin and Java bindings generated from `proto/`                   |
| `jvm/runtime`         | `chunk-runtime`       | session execution and Minestom integration behind the public API   |
| `jvm/build-api`       | `chunk-build-api`     | build extension contracts for chunk and overworld                  |
| `jvm/gradle-plugin`   | `dev.chunkzero.chunk` | maps the layout onto Gradle; driven by the `chunk` binary          |

The JVM SDK owns session execution and Minestom integration. Its runtime
keeps protobuf dependencies internal. A public API module will be introduced
with the first developer-facing SDK types. Overworld remains optional.

## TypeScript

`packages/edge` is `@chunk/edge`, the one module edge code imports. Its
declarations build descriptors; they do nothing at module initialization. It
targets ES2023 with no DOM and no Node types because the QuickJS runtime
supplies only the language. The bundler (Rolldown) is embedded in
`chunk-build`; apps never configure it.

`apps/dashboard` is a React/Vite CSR application using TanStack Router and
Query. The binary serves its built files and `/api/status` on an optional
management listener alongside Minecraft TCP. All current management API data
requires an operator bearer token. Only status exists; lifecycle controls,
asset uploads, and live event subscriptions are not implemented yet.

## Transport

`proto/chunk/v1/` is the source of truth for the internal transport, shared
by `crates/chunk-proto` and `jvm/proto` and linted with buf. Function
arguments, results, session params and attachments cross as `bytes` in the
contract's wire encoding, so the encoders are generated from the same
validators as the types. Player frames are one packet per message with no
length prefix, compression or encryption.

## Decisions made in this repository

- **R1.** One binary for toolchain and platform. Roles are subcommands.
- **R2.** The session process is always the gRPC client. Runtime commands,
  delivery and withdrawal travel down the `Attach` stream (see above).
- **R3.** Host backends live inside `chunk-control` as modules, not as
  separate crates, until one needs a dependency the others should not carry.
- **R4.** The proxy and the app runtime are separate crates joined by
  `chunk-edge`, so connection ownership can be tested without JavaScript and
  the runtime without sockets.
- **R5.** buf's service-suffix and request/response naming rules are off.
  Services are named for what they are.

## Open here

Platform-level questions that shape this repository and are still open:
what a session does when the edge is unreachable, registry consistency
across processes, backend recovery and future distribution, the inward frame
format, and transfer cookie keys. Specific to this repository:

- Type checking edge code needs a TypeScript compiler. Whether `chunk build`
  shells out to a Node install, embeds `tsgo`, or skips checking in `run` is
  undecided; the executor only bundles.
- Whether `chunk-runtime` relays `EdgeCall` to the edge or the process dials
  the edge directly for it. The one connection rule says relay; latency may
  argue otherwise.
- Where the directory's state lives when `chunk control` is one process, and
  what happens to placement when it is not reachable.
