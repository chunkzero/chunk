# Self-hosting with Compose

`compose.yaml` runs chunk on one Docker or Podman host: Postgres, the management service (with the dashboard), and the
edge that players connect to. Management starts each environment's machines itself, through the engine socket, on the
`chunk` network the bundle declares.

## Security

- **The engine socket is root-equivalent.** Management can start any container with any mount. With rootful Docker that
  means root on the host; with rootless Podman it means the account that runs Podman. Treat management's operator token
  and `CHUNK_SECRET_KEY` accordingly.
- **Self-hosting is single-host and single-tenant.** Every environment's machines share one engine and one network, and
  run next to Postgres and management. Only deploy code you trust.
- Gateways accept PROXY headers, which carry players' addresses, from the edge's static address (`10.231.0.2`) only. The
  network uses `10.231.0.0/16`; if that collides with a network the host reaches, change the subnet, the edge's address
  and `CHUNK_MACHINE_TRUSTED_EDGES` together.

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
on the `PATH`.

These settings can be added to `.env`:

| Variable                   | Default                   | Purpose                                                                |
| -------------------------- | ------------------------- | ---------------------------------------------------------------------- |
| `CHUNK_PLAYER_PORT`        | `25565`                   | The host port players connect to.                                      |
| `CHUNK_EDGE_DOMAIN`        | `localhost`               | Environments get `env-<id>.<domain>`; point `*.<domain>` at this host. |
| `CHUNK_MANAGEMENT_PUBLISH` | `127.0.0.1:8080`          | Where the host publishes management's API and dashboard.               |
| `CHUNK_PUBLIC_URL`         | `http://localhost:8080`   | How clients reach management, used in upload and login URLs.           |
| `CHUNK_ENVIRONMENT_IMAGE`  | `chunk-environment:0.1.0` | The image core and gateway machines run.                               |
| `CHUNK_JVM_IMAGE`          | `chunk-jvm:{java}`        | The image JVM machines run, `{java}` being the release's Java version. |
| `CHUNK_EDGE_IMAGE`         | `chunk-edge:0.1.0`        | The edge's image.                                                      |
| `CHUNK_MANAGEMENT_IMAGE`   | `chunk-management:0.1.0`  | Management's image.                                                    |
| `CHUNK_ENGINE_SOCKET`      | `/var/run/docker.sock`    | The host's engine socket; `init.sh` detects it.                        |
| `CHUNK_ENGINE_GID`         | `0`                       | The socket's group inside management: the `docker` group's ID, or `0`. |

Log replication isn't part of the bundle; add the `CHUNK_LOG_STORE_*` variables (see the management README) to
management in a `compose.override.yaml`.

## Reaching management

The dashboard is at `CHUNK_PUBLIC_URL`. The API takes the operator token from `.env` as a bearer token, over Connect
JSON:

```sh
token=$(sed -n 's/^CHUNK_OPERATOR_TOKEN=//p' .env)
curl -sS http://localhost:8080/chunk.management.v1.ProjectService/ListProjects \
  -H "authorization: Bearer $token" -H 'content-type: application/json' -d '{}'
```

To reach it from elsewhere, put a TLS reverse proxy in front and set `CHUNK_PUBLIC_URL` to its URL.

## Stopping

Delete environments before `compose down`: their machines are not part of the Compose project and keep the `chunk`
network in use. `compose down -v` also deletes the database and stored releases.
