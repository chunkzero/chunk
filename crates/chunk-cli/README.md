# chunk CLI

`chunk` is the command-line tool for a chunk project: it scaffolds projects, generates the TypeScript SDK and backend
clients, builds releases, runs a project locally with `chunk dev`, and operates that local environment. It embeds the
compiler and release packaging from [`chunk-build`](../chunk-build/README.md) and runs core and the gateway in-process
through [`chunk-environment`](../chunk-environment/README.md). It does not deploy; self-hosted releases are deployed
through the management API (see [`deploy/compose`](../../deploy/compose/README.md)).

From a checkout, `just toolchain` builds `target/debug/chunk` and installs the pinned native TypeScript compiler beside
it; `chunk --help` and `chunk <command> --help` list every option. [`docs/distribution.md`](../../docs/distribution.md)
covers the packaged SDK.

## Commands

| Command                           | What it does                                                                                |
| --------------------------------- | ------------------------------------------------------------------------------------------- |
| `chunk create DIR`                | Creates a project with one `lobby` app. `--language java` or `kotlin` (the default).        |
| `chunk codegen [PROJECT]`         | Writes the schema-aware TypeScript SDK into `PROJECT/.chunk/` for editors, without a build. |
| `chunk build [PROJECT]`           | Builds the backend and every app into a release, `dist/<release id>.tar.gz`.                |
| `chunk dev [PROJECT]`             | Builds the project and runs it locally, rebuilding on change. `chunk local` is an alias.    |
| `chunk clean [PROJECT]`           | Deletes `dist/` and `.chunk/` output, keeping local backend data unless `--data` is passed. |
| `chunk gen [PROJECT] --target T`  | Compiles the backend and generates a `java`, `kotlin` or `typescript` client for it.        |
| `chunk inspect [PROJECT]`         | Prints the project and app manifests as JSON, without building.                             |
| `chunk players --player UUID ...` | Moves a player to another session, or drains the JVM they are on, in a running `chunk dev`. |
| `chunk nodes ...`                 | Lists the JVMs of a running `chunk dev` as JSON, or shuts one down.                         |
| `chunk auth login`, `chunk login` | Records which platform later commands will use. Logging in itself is not implemented yet.   |
| `chunk auth status`               | Shows the recorded platform.                                                                |

`PROJECT` defaults to the current directory. Every command except `create` and `codegen` needs the project's
`chunk.toml`.

### `create`

`chunk create DIR` writes `chunk.toml`, a TypeScript backend under `server/`, `apps/scope.ts`, an `apps/lobby` app in
Java or Kotlin, and a Gradle build with the wrapper, then prints the next commands. `DIR` must be new or empty. The
project's Gradle plugin and JVM libraries come from the SDK's Maven repository; until they are published, point the
project at a checkout with `--chunk-source PATH` (or `CHUNK_SOURCE`), which includes the checkout's Gradle builds. The
CLI and the checkout must have the same version.

### `build`

`chunk build` runs the project's Gradle wrapper with `chunkArtifacts`, passing its own executable as
`-Pchunk.executable`, so Gradle's backend compilation and client generation use the same CLI. It then publishes the
release directory and its archive into `PROJECT/dist`, or into `--output DIR`, and prints the archive's path. A failed
or cancelled Gradle build publishes nothing. [`chunk-build`](../chunk-build/README.md) describes what a release holds.

### `dev`

`chunk dev` builds a development release, then runs the environment on this machine: core (the backend and control) and
the gateway in one process, and each app's JVMs as child processes. Players join at `--bind` (default
`127.0.0.1:25565`); control listens on `--control-bind` (default `127.0.0.1:25567`). Both must be fixed loopback
addresses. The project's `chunk.toml` needs a `[local]` section, which sets the environment name, profiles and process
limits (see [`chunk-build`](../chunk-build/README.md#project-manifest)).

State lives in `PROJECT/.chunk/local`, or `--state DIR`: the backend's data, published releases, JVM logs under
`control/nodes/`, and `control.json`, which `chunk players` and `chunk nodes` read to reach control. One `chunk dev`
runs per project at a time. `--java PATH` overrides the Java executable Gradle selected; it must satisfy the release's
Java version. `--offline-logins` admits players without Mojang authentication, under offline-mode UUIDs; use it only for
local testing.

`chunk dev` watches the project's sources, except build output, `.chunk`, `dist`, `node_modules` and hidden files, and
rebuilds 300 ms after the last change (`--no-watch` rebuilds only on request). A new release starts beside the running
one and new players go to it:

- If only backend code changed, existing sessions stay on the previous release until their players leave.
- If an app's JAR changed, earlier releases drain: they stop once empty for 10 seconds, or after `--drain-seconds`
  (default 30, at most 120), disconnecting the players still on them.
- A failed build leaves the previous release serving.

A forced restart (`r`) rebuilds and replaces every running release at once, ending their sessions.

In a terminal, `chunk dev` shows a UI with tabs for its own steps, the build, the proxy, control, the backend, the JVMs
and the players. `←`/`→` switch tabs, `/` searches, `b` toggles details, `r` restarts, and `q` or Ctrl-C quits. On the
JVM tab, `Tab` switches between the node list and the selected node's logs; on the players tab, `m` moves the selected
player to another destination. `--plain` prints one line per step instead, and is automatic when stdout is not a
terminal; type `r` and Enter to restart. Quitting stops the gateway, then waits for every JVM to confirm its exit; a
second Ctrl-C exits at once, and the next `chunk dev` stops any JVM left running.

### `clean`

`chunk clean` deletes `dist/` and everything under `.chunk/` except the local backend data of each `chunk dev` state
directory; `--data` deletes that too. It refuses while `chunk dev` runs for the project, and never follows a symlink.

### `gen`, `codegen` and `inspect`

`chunk codegen` writes the SDK and the types generated from the project's schema into `.chunk/sdk/` and
`.chunk/generated/`, so editors resolve `#chunk` imports. `chunk gen --target java|kotlin|typescript` compiles the
backend into `--backend-output` (default `PROJECT/.chunk/build/backend`) and generates a client into `--output` (default
`PROJECT/.chunk/generated/<target>`); `--java-package` sets the Java and Kotlin package, by default
`dev.chunkzero.generated`. The Gradle plugin runs `chunk gen` during `chunk build`, so projects rarely call it directly.
`chunk inspect` prints what the build tools read from `chunk.toml` and the `app.ts` declarations. See
[`chunk-build`](../chunk-build/README.md) for the output of each.

### `players` and `nodes`

These call control on a running `chunk dev` through the credential in `--control-file`, by default
`.chunk/local/control.json` relative to the current directory. `just players ...` fills it in for `examples/local`.

```sh
chunk players --player <uuid> move --session-type arena --key arena
chunk players --player <uuid> drain --timeout-seconds 60
chunk nodes list
chunk nodes shutdown <host> --timeout-seconds 60
```

`move` queues a move of the player, on their existing connection, to a session of that session type and key
(`--machine-profile` defaults to `local`). `drain` (the player's JVM, 10 to 120 seconds) and `nodes shutdown` (a host by
ID from `nodes list`, 0 to 120 seconds) stop new placement on a JVM, move its players off, and stop it once it is empty
or at the deadline; zero seconds stops it at once. `drain` waits until the JVM has stopped, giving up 30 seconds past
the deadline; `nodes shutdown` prints the node and returns. Each prints its operation ID; pass it back with
`--operation ID` (before the subcommand) when retrying a command whose outcome is unknown, so it is not applied twice.

### `auth`

`chunk auth login --cloud` or `--url URL` records the platform in `target.json` under `CHUNK_CONFIG_DIR`, by default the
user's configuration directory plus `chunk/`. `CHUNK_API_URL` overrides the recorded platform. No command uses it yet.

## Testing

`cargo test -p chunk-cli` runs the CLI's tests. `just consumers` checks the CLI as a user would meet it: it copies the
checkout into a temporary directory, creates new Java and Kotlin projects with `chunk create --chunk-source`, runs
`codegen` and `build` on them and on both examples, and verifies each release.
