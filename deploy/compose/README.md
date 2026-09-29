# Self-hosting with Compose

`compose.yaml` runs chunk on one Docker or Podman host: Postgres, [management](../../packages/management/README.md) with
its dashboard, and the [edge](../../crates/chunk-edge/README.md) that players connect to. Management starts each
environment's machines itself, through the engine's socket, on the `chunk` network the bundle declares.

## Security

- **The engine socket is root-equivalent.** Management can start any container with any mount. With rootful Docker that
  means root on the host; with rootless Podman it means the account that runs Podman. Guard management's operator token
  and `CHUNK_SECRET_KEY` accordingly.
- **Self-hosting is single-host and single-tenant.** Every environment's machines share one engine and one network, next
  to Postgres and management. Only deploy code you trust.
- Gateways accept PROXY headers, which carry players' addresses, only from the edge's static address (`10.231.0.2`). The
  network uses `10.231.0.0/16`; if that collides with a network the host reaches, change the subnet, the edge's address
  and `CHUNK_MACHINE_TRUSTED_EDGES` in `compose.yaml` together.
- **Players' addresses need rootful Docker or rootful Podman.** Rootful engines forward the published IPv4 port with NAT
  and keep the player's address. Rootless Podman forwards through `rootlessport`, so every player arrives from the
  bridge's address, and per-player address limits and logs see one address. The player port is published on IPv4 only,
  since Docker's userland proxy hides IPv6 players the same way.

## Running

From the repository root, build the images:

```sh
just image && just jvm-image 25 && just edge-image && just management-image
```

Then, in this directory:

```sh
./init.sh             # writes .env (mode 600) with fresh secrets and the engine socket; never overwrites one
docker compose up -d  # or: podman compose up -d
```

Keep `.env`: `CHUNK_SECRET_KEY` encrypts stored secrets, so it must stay the same for the database's life. For rootless
Podman, enable its socket first (`systemctl --user enable --now podman.socket`); `init.sh` uses it when
`/var/run/docker.sock` doesn't exist. `podman compose` needs a compose provider, `docker-compose` or `podman-compose`,
on the `PATH`. The `chunk` network has a fixed name, so one engine runs one install.

These settings can be added to `.env`:

| Variable                   | Default                   | Meaning                                                                                      |
| -------------------------- | ------------------------- | -------------------------------------------------------------------------------------------- |
| `CHUNK_PLAYER_PORT`        | `25565`                   | The host port players connect to.                                                            |
| `CHUNK_PLAYER_BIND`        | `0.0.0.0`                 | The host IPv4 address the player port is published on.                                       |
| `CHUNK_EDGE_DOMAIN`        | `localhost`               | Environments get the hostname `env-<24 hex digits>.<domain>`; point `*.<domain>` here.       |
| `CHUNK_MANAGEMENT_PUBLISH` | `127.0.0.1:8080`          | Where the host publishes management's API and dashboard.                                     |
| `CHUNK_PUBLIC_URL`         | `http://localhost:8080`   | How clients reach management; used in upload and login URLs. Keep it in step with the above. |
| `CHUNK_ENVIRONMENT_IMAGE`  | `chunk-environment:0.1.0` | The image core and gateway machines run.                                                     |
| `CHUNK_JVM_IMAGE`          | `chunk-jvm:{java}`        | The image JVM machines run, `{java}` being the release's Java version.                       |
| `CHUNK_EDGE_IMAGE`         | `chunk-edge:0.1.0`        | The edge's image.                                                                            |
| `CHUNK_MANAGEMENT_IMAGE`   | `chunk-management:0.1.0`  | Management's image.                                                                          |
| `CHUNK_ENGINE_SOCKET`      | `/var/run/docker.sock`    | The host's engine socket; `init.sh` detects it.                                              |
| `CHUNK_ENGINE_GID`         | `0`                       | The socket's group inside management: the `docker` group's ID, or `0`.                       |

Other management settings go in a `compose.override.yaml` next to `compose.yaml`, which Compose reads automatically. For
example, to let idle environments sleep until a player logs in:

```yaml
services:
  management:
    environment:
      CHUNK_MACHINE_SUSPEND_AFTER_SECONDS: "600"
```

Log storage (the `CHUNK_LOG_STORE_*` variables) is added the same way.

## Reaching management

The dashboard is at `CHUNK_PUBLIC_URL`; sign in with the operator token from `.env`. The API takes the same token as a
bearer token, over Connect JSON:

```sh
url=http://localhost:8080  # CHUNK_PUBLIC_URL
token=$(sed -n 's/^CHUNK_OPERATOR_TOKEN=//p' .env)
curl -sS "$url/chunk.management.v1.ProjectService/ListProjects" \
  -H "authorization: Bearer $token" -H 'content-type: application/json' -d '{}'
```

To reach it from elsewhere, put a TLS reverse proxy in front and set `CHUNK_PUBLIC_URL` to its URL.

## Deploying

Build a release with `chunk build`, which prints the archive's path, `dist/<release id>.tar.gz`. Then create a project
and an environment, upload the release and deploy it. With `url` and `token` from above, `jq` and `uuidgen`:

```sh
rpc() {
  curl -fsS "$url/chunk.management.v1.$1" \
    -H "authorization: Bearer $token" -H 'content-type: application/json' -d "$2"
}
project=$(rpc ProjectService/CreateProject "{\"requestId\":\"$(uuidgen)\",\"name\":\"my-server\"}" | jq -r .project.id)
environment=$(rpc ProjectService/CreateEnvironment \
  "{\"requestId\":\"$(uuidgen)\",\"projectId\":\"$project\",\"name\":\"prod\"}" | jq -r .environment.id)

archive=path/to/dist/RELEASE.tar.gz  # the path chunk build printed
release=$(basename "$archive" .tar.gz)
upload=$(rpc DeploymentService/UploadRelease "{\"projectId\":\"$project\",\"releaseId\":\"$release\",
  \"archiveSha256\":\"$(sha256sum "$archive" | cut -d' ' -f1)\",\"archiveSizeBytes\":\"$(wc -c <"$archive" | tr -d ' ')\"}")
curl -fsS -X PUT -H 'content-type: application/gzip' --data-binary @"$archive" "$(echo "$upload" | jq -r .upload.url)"
rpc DeploymentService/CompleteReleaseUpload "{\"projectId\":\"$project\",\"releaseId\":\"$release\"}"
deployment=$(rpc DeploymentService/Deploy \
  "{\"requestId\":\"$(uuidgen)\",\"environmentId\":\"$environment\",\"releaseId\":\"$release\"}" | jq -r .deployment.id)
```

`UploadRelease` returns no `upload` when the project already holds the release; skip the `PUT` then. Follow the
deployment in the dashboard, or until it is `DEPLOYMENT_STATE_ACTIVE`:

```sh
rpc DeploymentService/GetDeployment "{\"deploymentId\":\"$deployment\"}" | jq -r .deployment.state
rpc ProjectService/GetEnvironment "{\"environmentId\":\"$environment\"}" | jq -r .environment.hostname
```

Players join at the environment's hostname and the player port, for example `env-<id>.localhost:25565` from this host
with the default domain. The edge routes by that hostname, so a bare IP address or `localhost` reaches no environment.
Deploy later releases to the same environment the same way.

`just managed-smoke`, from the repository root, runs this whole flow on a throwaway install with offline test players,
and removes everything it created; it refuses to run while another install uses the engine.

## Stopping

Delete environments before `compose down`: their machines are not part of the Compose project and keep the `chunk`
network in use.

Deletion finishes in the background, so wait until the environment is gone and none of its containers or volumes remain
before stopping management:

```sh
rpc ProjectService/DeleteEnvironment "{\"environmentId\":\"$environment\"}"
until ! rpc ProjectService/GetEnvironment "{\"environmentId\":\"$environment\"}" >/dev/null 2>&1 &&
  [ -z "$(docker ps -aq --filter "label=chunk.environment=$environment")" ] &&
  [ -z "$(docker volume ls -q --filter "label=chunk.environment=$environment")" ]; do
  sleep 2
done
docker compose down
```

Add `-v` to `compose down` only once no environment remains: it deletes the database and stored releases, including the
install ID that management uses to recognize and clean up its own machines.
