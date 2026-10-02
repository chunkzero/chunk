# chunk-environment

The environment process. It runs an environment's core (the [backend](../chunk-backend/README.md) and
[control](../chunk-control/README.md), with the `chunk.sync.v1` `Core` service) and its
[gateway](../chunk-proxy/README.md). `chunk dev` embeds the same `Core` and `Gateway` types; this crate's
`chunk-environment` binary runs them on machines that [management](../../packages/management/README.md) provides.

`CHUNK_SERVICES` picks what the process runs:

- `core,gateway` (the default): an environment's core machine.
- `gateway`: an extra gateway machine, which joins core on another machine.
- `core`: core without a player listener.

## Configuration

| Variable                              | Default                                 | Meaning                                                                                            |
| ------------------------------------- | --------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `CHUNK_SERVICES`                      | `core,gateway`                          | The services to run.                                                                               |
| `CHUNK_ENVIRONMENT_ID`                | required                                | The environment's ID; `CHUNK_ENVIRONMENT` is accepted too.                                         |
| `CHUNK_STATE`                         | required (`/data` in the image)         | Core's directory: the store, unpacked releases, control's files and `control.json`.                |
| `CHUNK_MANAGEMENT_URL`                | unset                                   | The management service that deploys this environment.                                              |
| `CHUNK_ENVIRONMENT_TOKEN`             | required with `CHUNK_MANAGEMENT_URL`    | The environment's bearer token for management.                                                     |
| `CHUNK_SUSPEND_AFTER_SECONDS`         | unset                                   | Seconds idle, at least 1, before core reports that it may be suspended. Never if unset.            |
| `CHUNK_BUNDLE`                        | required without `CHUNK_MANAGEMENT_URL` | A backend deployment to serve, such as a release's `backend.json`.                                 |
| `CHUNK_CONTROL_BIND`                  | `127.0.0.1:25567`                       | Core's loopback listener, for the gateway on this machine and for `chunk players` / `chunk nodes`. |
| `CHUNK_CORE_BIND`                     | unset                                   | Core's listener for other machines. It drops peers whose address is not loopback or private.       |
| `CHUNK_PRIVATE_ADDRESS`               | unset                                   | This machine's private address, where its JVMs serve players instead of loopback.                  |
| `CHUNK_CORE_ENDPOINT`                 | required for `gateway` alone            | Core's endpoint, `http://<private address>:<port>`.                                                |
| `CHUNK_GATEWAY_CREDENTIAL`            | required for `gateway` alone            | The machine credential core minted for this gateway.                                               |
| `CHUNK_BIND`                          | `0.0.0.0:25565`                         | The gateway's player listener.                                                                     |
| `CHUNK_MOTD`                          | `chunk`                                 | The gateway's server-list message.                                                                 |
| `CHUNK_MAX_CONNECTIONS`               | `1024`                                  | The gateway's concurrent connection limit.                                                         |
| `CHUNK_CONNECTION_TIMEOUT_SECONDS`    | `10`                                    | Seconds, 1 to 3600, a player's login exchange may take, authentication included.                   |
| `CHUNK_CONFIGURATION_TIMEOUT_SECONDS` | `300`                                   | Seconds, 1 to 3600, a player may wait in the configuration phase for a destination.                |
| `CHUNK_TRUSTED_EDGES`                 | unset                                   | Edge IPs or CIDRs, comma-separated, whose connections must open with a PROXY protocol v2 header.   |
| `CHUNK_OFFLINE_LOGINS`                | unset                                   | `1` admits players without Mojang authentication, under any name. Insecure; for tests only.        |
| `CHUNK_REPLICATION_BUCKET`            | unset                                   | An S3-compatible bucket core replicates its store to. Unset turns replication off.                 |
| `CHUNK_REPLICATION_ACCESS_KEY_ID`     | required with a bucket                  | The bucket's credentials.                                                                          |
| `CHUNK_REPLICATION_SECRET_ACCESS_KEY` | required with a bucket                  | The secret for the access key.                                                                     |
| `CHUNK_REPLICATION_REGION`            | `us-east-1`                             | The bucket's region.                                                                               |
| `CHUNK_REPLICATION_ENDPOINT`          | AWS                                     | The endpoint URL, for other S3-compatible stores; an `http://` URL allows plain HTTP.              |
| `CHUNK_REPLICATION_PREFIX`            | unset                                   | A key prefix within the bucket.                                                                    |
| `RUST_LOG`                            | `info`                                  | The log filter.                                                                                    |

chunk sends credentials between machines in the clear, so `CHUNK_CORE_BIND` belongs on a private, encrypted network,
such as WireGuard; prefer that network's address to an unspecified one.

With replication on, every write transaction is uploaded to the bucket in batches, with periodic snapshots. A core that
starts without a local store restores the latest state from the bucket under a new epoch, and an older core writing to
the same prefix is fenced and stops. Failed uploads are logged and retried; commits keep succeeding locally meanwhile.
Storage errors name the operation, key, HTTP status and S3 error code, never the response body, which can echo the
signed request. On shutdown, core flushes the log and exits non-zero if that final flush fails, so a clean exit means
the bucket holds every commit.

## Under management

With `CHUNK_MANAGEMENT_URL`, core attaches to management (`EnvironmentService.Attach`) and serves the deployment its
desired state names. Before it opens its store, core reads the first desired state from an attach that takes no lease:
when it grants a log store, core replicates there instead of to the `CHUNK_REPLICATION_*` bucket, restoring an empty
store from it, and then attaches as core with the epoch it serves. Every later desired state renews the log store's
credentials in place: those of that first attach, kept open while core restores and starts, then those of the core
attach, and after it ends, those of another attach that takes no lease, until the final flush. A log store moved
elsewhere takes effect only when core starts again. Core waits for management before opening its store, and exits
non-zero if stopped before management answered. It downloads each release archive, unpacks it under
`$CHUNK_STATE/releases/`, verifies it with the same checks as `chunk build`, makes it the backend's and control's
current release, and reports the deployment `ACTIVE`. A release it rejects is reported `FAILED`, and the previous
deployment keeps serving. The gateway starts with the first active deployment. Core retires the versions it no longer
needs, stopping their JVMs first, and removes their unpacked releases. JVMs run on machines core asks management for
(`EnsureCapacity`), each running [`chunk-jvm`](../chunk-jvm/README.md). A core that management fences stops.

Core reports its status about every 15 seconds, and at once when it changes. Each report carries the server-list status
the gateway last answered for each hostname, which the [edge](../chunk-edge/README.md) answers pings with while the
environment sleeps. Every five seconds, core reports the clients that failed authentication (`ReportFailedAuth`), so
management keeps them from waking the environment for a while.

With `CHUNK_SUSPEND_AFTER_SECONDS`, core reports `ready_to_suspend` once nothing has been active for that long: no
action, hook, command or job in the backend, no gateway holding connections (server-list pings and logins in progress
included), no open claim or launching JVM in control, and no deployment loading. Replication must also have flushed, no
job may be due within the grace period, and the backend's next due job must be handed to management as a wake alarm
(`SetWakeAlarm`). Queries, mutations and operator calls don't count as activity. Any activity starts the grace period
over.

## Gateway machines

With `CHUNK_SERVICES=gateway`, the process runs no core and reads no `CHUNK_STATE` or `CHUNK_BUNDLE`. It joins core at
`CHUNK_CORE_ENDPOINT` with `CHUNK_GATEWAY_CREDENTIAL`, whose scope must name `CHUNK_ENVIRONMENT_ID`. It follows core's
current deployment: its player listener starts once a release is current, and later players go to each new one. Core
revoking the credential stops the process.

## Without management

With `CHUNK_BUNDLE` instead of `CHUNK_MANAGEMENT_URL`, core serves that backend deployment, but control gets no release,
so the gateway cannot place players in sessions. Use `chunk dev` to run a whole project on one machine.

SIGTERM or SIGINT stops the gateway, then control and its JVMs, then the backend.

## Container image

`just image` builds `chunk-environment:<workspace version>` from `Dockerfile` with Podman or Docker. The image runs as a
non-root user, sets `CHUNK_STATE=/data` on a volume, and exposes the gateway's port, 25565. Management starts it for
core and gateway machines; see [`deploy/compose`](../../deploy/compose/README.md).

## Testing

`cargo test -p chunk-environment` runs the crate's tests. `just jvm-e2e` checks a managed core that launches its JVM in
the `chunk-jvm` image; see [`chunk-jvm`](../chunk-jvm/README.md#testing).
