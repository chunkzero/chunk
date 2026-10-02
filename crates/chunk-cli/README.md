# chunk CLI

`chunk` is the command-line tool for a chunk project: it scaffolds projects, generates the TypeScript SDK and backend
clients, builds releases, runs a project locally with `chunk dev`, operates that local environment, and deploys releases
to a platform: a [self-hosted install](../../deploy/compose/README.md) or Chunk Cloud. It embeds the compiler and
release packaging from [`chunk-build`](../chunk-build/README.md) and runs core and the gateway in-process through
[`chunk-environment`](../chunk-environment/README.md).

From a checkout, `just toolchain` builds `target/debug/chunk` and installs the pinned native TypeScript compiler beside
it; `chunk --help` and `chunk <command> --help` list every option. [`docs/distribution.md`](../../docs/distribution.md)
covers the packaged SDK.

## Commands

| Command                              | What it does                                                                                   |
| ------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `chunk create DIR`                   | Creates a project with one `lobby` app. `--language java` or `kotlin` (the default).           |
| `chunk codegen [PROJECT]`            | Writes the schema-aware TypeScript SDK into `PROJECT/.chunk/` for editors, without a build.    |
| `chunk build [PROJECT]`              | Builds the backend and every app into a release, `dist/<release id>.tar.gz`.                   |
| `chunk dev [PROJECT]`                | Builds the project and runs it locally, rebuilding on change. `chunk local` is an alias.       |
| `chunk clean [PROJECT]`              | Deletes `dist/` and `.chunk/` output, keeping `chunk dev` backend data unless `--data`.        |
| `chunk gen [PROJECT] --target T`     | Compiles the backend and generates a `java`, `kotlin` or `typescript` client for it.           |
| `chunk inspect [PROJECT]`            | Prints the project and app manifests as JSON, without building.                                |
| `chunk players --player UUID ...`    | Moves a player to another session, or drains the JVM they are on, in a running `chunk dev`.    |
| `chunk nodes ...`                    | Lists the JVMs of a running `chunk dev` as JSON, or shuts one down.                            |
| `chunk auth login`, `chunk login`    | Logs in to a platform, approving the login in its dashboard.                                   |
| `chunk auth status`                  | Shows the platform and who you are logged in as.                                               |
| `chunk auth logout`                  | Revokes the CLI's token and forgets it.                                                        |
| `chunk projects [create NAME]`       | Lists the platform's projects, or creates one; `--owner` picks its team when you have several. |
| `chunk environments [create NAME]`   | Lists a project's environments with their state and join address, or creates one.              |
| `chunk environments delete E`        | Deletes an environment, destroying its machines and data.                                      |
| `chunk environments fork N --from E` | Creates an environment from a snapshot of another, running the release that data was serving.  |
| `chunk environments snapshots E`     | Lists the snapshots stored in an environment's log, newest first.                              |
| `chunk deploy [PROJECT] --env E`     | Builds the project, uploads and deploys its release, and waits until it is active.             |
| `chunk deployments --env E`          | Lists an environment's recent deployments, newest first.                                       |
| `chunk apps --env E`                 | Lists the apps and session types the environment's active release runs.                        |
| `chunk logs --env E`                 | Prints an environment's logs; `--follow` keeps printing new ones.                              |
| `chunk secrets put NAME --env E`     | Sets a secret from a hidden prompt, or from stdin without a terminal.                          |
| `chunk secrets list --env E`         | Lists an environment's secret names and versions, never their values.                          |
| `chunk secrets delete NAME --env E`  | Deletes a secret.                                                                              |
| `chunk promote --from E1 --env E2`   | Deploys the release active in `E1` to `E2`, and waits until it is active.                      |
| `chunk rollback --env E`             | Deploys the release of an earlier deployment again; `--to ID` picks it.                        |
| `chunk domains add HOST --env E`     | Claims a custom hostname and prints the DNS records to create.                                 |
| `chunk domains verify HOST --env E`  | Checks the domain's DNS records now, verifying it when they match.                             |
| `chunk domains list --env E`         | Lists an environment's custom domains.                                                         |
| `chunk domains remove HOST --env E`  | Removes a domain and its route.                                                                |

`PROJECT` defaults to the current directory. The commands that take `PROJECT`, except `create` and `codegen`, need its
`chunk.toml`. The commands from `auth` down call a platform's management API.

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
local testing. `--connection-timeout-seconds` (default 10) and `--configuration-timeout-seconds` (default 300), each
from 1 to 3600, set the gateway's login and configuration deadlines.

Backend functions and the apps' generated `Vars` read `chunk.toml`'s top-level `[vars]`, or with `--env NAME` its
`[env.NAME.vars]` over them, key by key. Actions read their secrets from `.dev.vars` in the project root: `NAME=value`
lines, with `#` comments and optional quotes. Keep it out of version control, as the project template's `.gitignore`
does. `chunk dev` reads it on every rebuild, editing it triggers one, and it warns, by name only, about secrets
`[secrets] required` lists that it lacks.

`chunk dev` watches the project's sources, except build output, `.chunk`, `dist`, `node_modules` and hidden files other
than `.dev.vars`, and rebuilds 300 ms after the last change (`--no-watch` rebuilds only on request). A new release
starts beside the running one and new players go to it:

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

`chunk clean` deletes `dist/` and everything under `.chunk/` except the backend data of `chunk dev` state directories
directly under `.chunk/` (such as the default `.chunk/local`); `--data` deletes that too. A `--state` directory anywhere
else is not recognized, so its data is deleted if it lies under `.chunk/` or `dist/`. It refuses while `chunk dev` runs
for the project, and never follows a symlink.

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
`--operation ID` (before the subcommand for `players`, after it for `nodes shutdown`) when retrying a command whose
outcome is unknown, so it is not applied twice.

### `auth`

`chunk auth login --url URL` logs in to a self-hosted install's management at `URL`, and `--cloud` to Chunk Cloud;
without either, it asks in a terminal. It prints a link to the platform's dashboard and a code: open the link, signed
in, and approve the login. The CLI then saves the platform and the token it issued under the config directory,
`CHUNK_CONFIG_DIR` or by default the user's configuration directory plus `chunk/`, and sends that token only to that
platform. `chunk auth status` shows the platform and who you are logged in as; `chunk auth logout` revokes the token and
forgets it.

Two variables override the saved login, for scripts and CI. `CHUNK_API_URL` selects another platform URL, which gets no
saved token unless it is the saved platform's. `CHUNK_TOKEN` is the token to use instead, sent to `CHUNK_API_URL` or
else the saved platform; `chunk auth logout` never revokes it.

### `deploy`, `promote`, `rollback`, `projects`, `environments`, `deployments`, `apps`, `logs`, `secrets` and `domains`

```sh
chunk projects create my-server
chunk environments create prod
chunk deploy --env prod
chunk deployments --env prod
```

`--project` (or `CHUNK_PROJECT`) selects a project by name or ID, and may be left out while there is only one; `--env`
selects an environment of it the same way. Project and environment names are 1 to 63 lowercase letters, digits and
hyphens, starting and ending with a letter or digit.

`chunk deploy` builds the project as `chunk build` does, uploads the release unless the project already holds it,
deploys it, and follows the deployment until it is active, then prints where players join. It fails if the deployment
fails or a later one supersedes it first. Ctrl-C stops waiting but not the deployment; `chunk deployments` shows how it
ends (`--limit`, default 20, lists up to 200). Before uploading, it warns about secrets `chunk.toml` requires that the
environment has no value for.

`chunk promote --from SOURCE --env TARGET` deploys the release active in `SOURCE` to `TARGET`, an environment of the
same project, and follows the deployment as `deploy` does. Only the release moves, never data or secrets.
`chunk rollback --env E` deploys the release of the deployment active before the current one again, or of the one
`--to DEPLOYMENT_ID` names, which `chunk deployments` lists.

`chunk environments delete NAME_OR_ID` asks for confirmation on a terminal, and needs `--yes` without one, since it
destroys the environment's machines and data. Deleting finishes in the background; `--wait` polls until the platform
reports the environment gone, for up to five minutes, and Ctrl-C stops waiting but not the deletion.

`chunk environments fork NAME --from E` creates an environment from the source's latest state, or from the snapshot
`--snapshot ID` names, which `chunk environments snapshots E` lists. The fork has the source's documents and runs the
release its restored data was serving, deployed once the fork's core has restored it; when that data served none, the
fork runs nothing until `chunk deploy`. The source's scheduled jobs are dropped, and its secrets are copied only with
`--copy-secrets`. The platform keeps snapshots for a limited time, so a fork from an older snapshot can fail to start
once it is pruned.

`chunk logs` prints the most recent entries (`--limit`, default 200), and with `--follow` keeps printing new ones;
`--app ID` keeps only that app's JVM entries. A self-hosted install has none to show yet, since its environments don't
send their logs to management.

`chunk secrets put NAME --env E` sets a secret's value for a name `chunk.toml`'s `[secrets] required` lists, which
backend actions read through `ctx.env.NAME`. It reads the value from a hidden prompt on a terminal, or else from stdin
without its final line break, as in `printf %s "$TOKEN" | chunk secrets put API_TOKEN --env prod`. Each change is a new
version, and running deployments receive it without a redeploy. Values are non-empty UTF-8 up to 64 KiB, and an
environment holds up to 256 secrets. `chunk secrets list` shows names and versions only; no command prints a value.
`chunk secrets delete NAME` asks for confirmation on a terminal and needs `--yes` without one.

`chunk domains add HOSTNAME --env E` claims a custom hostname and prints its state and the DNS records to create: a TXT
ownership proof and an SRV record that routes players to the environment. A hostname routes only once it is verified:
create the records, then run `chunk domains verify HOSTNAME --env E`, which checks DNS right away. `chunk domains list`
shows each domain's state, and `chunk domains remove HOSTNAME` removes a domain and its route. Commands that take a
domain accept its hostname or its ID.

## Testing

`cargo test -p chunk-cli` runs the CLI's tests. `just consumers` checks the CLI as a user would meet it: it copies the
checkout into a temporary directory, creates new Java and Kotlin projects with `chunk create --chunk-source`, runs
`codegen` and `build` on them and on both examples, and verifies each release.
