# Environment process

`chunk-environment` runs an environment's core and gateway services in one process. Core runs the backend functions
itself. `CHUNK_SERVICES` picks the services: `core,gateway` (the default), `core`, or `gateway` alone on a gateway
machine that joins core on another machine. A legacy `exec` is ignored with a warning.

## Container image

`just image` builds `chunk-environment:<workspace version>` from `Dockerfile` with podman or docker. The image runs as a
non-root user, keeps its state in the `/data` volume and exposes the gateway's player port, 25565.

```sh
podman run --rm -p 25565:25565 -v chunk-data:/data -v ./bundle.json:/bundle.json:ro \
  -e CHUNK_ENVIRONMENT_ID=dev -e CHUNK_BUNDLE=/bundle.json chunk-environment:0.1.0
```

It reads these variables:

| Variable                   | Default                                 | Meaning                                                                       |
| -------------------------- | --------------------------------------- | ----------------------------------------------------------------------------- |
| `CHUNK_SERVICES`           | `core,gateway`                          | The services to run.                                                          |
| `CHUNK_ENVIRONMENT_ID`     | required                                | The environment ID; `CHUNK_ENVIRONMENT` is accepted too.                      |
| `CHUNK_MANAGEMENT_URL`     | unset                                   | The management service that deploys the environment.                          |
| `CHUNK_ENVIRONMENT_TOKEN`  | required with `CHUNK_MANAGEMENT_URL`    | The environment's bearer token for the management service.                    |
| `CHUNK_BUNDLE`             | required without `CHUNK_MANAGEMENT_URL` | The backend deployment (a `chunk_contract::Deployment` as JSON) served first. |
| `CHUNK_STATE`              | `/data`                                 | The store, control's files, and the `control.json` record.                    |
| `CHUNK_CORE_ENDPOINT`      | required for `gateway` alone            | Core's network endpoint, `http://<private address>:<port>`.                   |
| `CHUNK_GATEWAY_CREDENTIAL` | required for `gateway` alone            | The machine credential core minted for this gateway.                          |
| `CHUNK_BIND`               | `0.0.0.0:25565`                         | The gateway's player listener.                                                |
| `CHUNK_MOTD`               | `chunk`                                 | The gateway's server list message.                                            |
| `CHUNK_MAX_CONNECTIONS`    | `1024`                                  | The gateway's connection limit.                                               |
| `CHUNK_CONTROL_BIND`       | `127.0.0.1:25567`                       | Control's gRPC listener; it must be loopback.                                 |
| `CHUNK_CORE_BIND`          | unset                                   | Core's listener for other machines; it drops peers that aren't private.       |
| `CHUNK_PRIVATE_ADDRESS`    | `FLY_PRIVATE_IP`                        | This machine's private address. Its JVMs serve players there, not loopback.   |
| `RUST_LOG`                 | `info`                                  | The log filter.                                                               |

Chunk sends credentials between machines in the clear, so `CHUNK_CORE_BIND` belongs on a private, encrypted network,
such as WireGuard or Fly's 6PN; prefer that network's address to an unspecified one. The listener serves the same `Core`
service as the loopback control listener, and drops connections from peers that aren't loopback or private.

## Gateway machines

With `CHUNK_SERVICES=gateway`, the process runs no core and reads no `CHUNK_STATE` or `CHUNK_BUNDLE`. It joins core at
`CHUNK_CORE_ENDPOINT` with `CHUNK_GATEWAY_CREDENTIAL`, whose scope names the environment and the gateway's ID; it stops
at once if that environment isn't `CHUNK_ENVIRONMENT_ID`. It follows core's `deployment` topic: the player listener
starts once a release is current, so no login is taken before then, and later players go to each new current release.
Core's revoking the credential stops the process.

## Under management

With `CHUNK_MANAGEMENT_URL`, core attaches to the management service (`EnvironmentService.Attach`) and serves the
deployment its desired state names. It downloads each release archive, unpacks it under `$CHUNK_STATE/releases/` and
verifies it with the same checks as `chunk build`, then makes it the backend's and control's current release and reports
the deployment `ACTIVE`. A release it rejects is reported `FAILED` and the previous deployment keeps serving. A newer
desired deployment cancels one still loading, which then never serves. The gateway starts with the first active
deployment. A core that management fences stops.

Once the first desired state arrives, core retires every version the backend holds except the latest desired one, the
one control serves, and the one it replaced until management accepts the replacement as `ACTIVE`, which
`$CHUNK_STATE/managed.json` keeps across restarts; that includes versions a restart or a rejected deployment left
behind. Retiring stops a version's JVMs, then releases it. An unpacked release is removed once control no longer runs or
may run its JVMs and no load uses it, and unfinished downloads and unpacks are removed at startup.

SIGTERM or SIGINT stops the gateway, then every JVM and control, then the backend.
