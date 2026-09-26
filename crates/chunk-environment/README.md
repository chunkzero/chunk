# Environment process

`chunk-environment` runs an environment's core, gateway and exec services in one process. `CHUNK_SERVICES` picks them:
`core,gateway,exec` (the default) or `core,exec`.

## Container image

`just image` builds `chunk-environment:<workspace version>` from `Dockerfile` with podman or docker. The image runs as a
non-root user, keeps its state in the `/data` volume and exposes the gateway's player port, 25565.

```sh
podman run --rm -p 25565:25565 -v chunk-data:/data -v ./bundle.json:/bundle.json:ro \
  -e CHUNK_ENVIRONMENT_ID=dev -e CHUNK_BUNDLE=/bundle.json chunk-environment:0.1.0
```

It reads these variables:

| Variable                  | Default                                 | Meaning                                                                        |
| ------------------------- | --------------------------------------- | ------------------------------------------------------------------------------ |
| `CHUNK_SERVICES`          | `core,gateway,exec`                     | The services to run.                                                           |
| `CHUNK_ENVIRONMENT_ID`    | required                                | The environment ID; `CHUNK_ENVIRONMENT` is accepted too.                       |
| `CHUNK_MANAGEMENT_URL`    | unset                                   | The management service that deploys the environment.                           |
| `CHUNK_ENVIRONMENT_TOKEN` | required with `CHUNK_MANAGEMENT_URL`    | The environment's bearer token for the management service.                     |
| `CHUNK_BUNDLE`            | required without `CHUNK_MANAGEMENT_URL` | The backend deployment (a `chunk_contract::Deployment` as JSON) served first.  |
| `CHUNK_STATE`             | `/data`                                 | The store, control's files, and the `backend.json` and `control.json` records. |
| `CHUNK_BIND`              | `0.0.0.0:25565`                         | The gateway's player listener.                                                 |
| `CHUNK_MOTD`              | `chunk`                                 | The gateway's server list message.                                             |
| `CHUNK_MAX_CONNECTIONS`   | `1024`                                  | The gateway's connection limit.                                                |
| `CHUNK_BACKEND_BIND`      | `127.0.0.1:25568`                       | The backend's gRPC listener.                                                   |
| `CHUNK_CONTROL_BIND`      | `127.0.0.1:25567`                       | Control's gRPC listener; it must be loopback.                                  |
| `RUST_LOG`                | `info`                                  | The log filter.                                                                |

## Under management

With `CHUNK_MANAGEMENT_URL`, core attaches to the management service (`EnvironmentService.Attach`) and serves the
deployment its desired state names. It downloads each release archive, unpacks it under `$CHUNK_STATE/releases/` and
verifies it with the same checks as `chunk build`, then makes it the backend's and control's current release and
reports the deployment `ACTIVE`. A release it rejects is reported `FAILED` and the previous deployment keeps serving. A
newer desired deployment cancels one still loading, which then never serves. The gateway starts with the first active
deployment. A core that management fences stops.

Once a deployment serves, core retires every other version the backend holds, including those a restart interrupted:
it stops their JVMs, then releases the version. An unpacked release is removed once control no longer runs or may run
its JVMs, and unfinished downloads and unpacks are removed at startup.

SIGTERM or SIGINT stops the gateway, then every JVM and control, then the backend.
