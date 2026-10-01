# Self-hosting with Compose

`compose.yaml` runs chunk on one Docker or Podman host: Postgres, [management](../../packages/management/README.md) with
its dashboard, and the [edge](../../crates/chunk-edge/README.md) that players connect to. Management starts each
environment's machines itself, through the engine's socket, on the `chunk` network the bundle declares.

## Security

- **The engine socket is root-equivalent.** Management can start any container with any mount: root on the host with
  rootful Docker, the Podman user's account with rootless Podman. Guard the operator token and `CHUNK_SECRET_KEY`
  accordingly.
- **One host, one tenant.** Every environment's machines share one engine and one network with Postgres and management.
  Only deploy code you trust.
- **Gateways trust PROXY headers from the edge alone**, at its static address `10.231.0.2`. If the network's
  `10.231.0.0/16` collides with one the host reaches, change the subnet, the edge's address and
  `CHUNK_MACHINE_TRUSTED_EDGES` in `compose.yaml` together.
- **Players' real addresses need a rootful engine**, which forwards the published IPv4 port with NAT. Rootless Podman
  forwards through `rootlessport`, so every player arrives from the bridge's address, and per-player limits and logs see
  only that. The player port is published on IPv4 only, since Docker's userland proxy hides IPv6 players the same way.

## Running

You need Docker or Podman with a compose provider (`podman compose` runs `docker-compose` or `podman-compose` from the
`PATH`), and the toolchain from the root README's [Getting started](../../README.md#getting-started). For rootless
Podman, enable its socket: `systemctl --user enable --now podman.socket`.

From the repository root, build the images:

```sh
just image && just jvm-image 25 && just edge-image && just management-image
```

Then, in this directory, create `.env` and start the stack:

```sh
cp .env.example .env && chmod 600 .env  # then fill in the secrets and the engine socket, as its comments show
docker compose up -d                    # or: podman compose up -d
```

Keep `.env` for the life of the install: `CHUNK_SECRET_KEY` encrypts stored secrets. [`.env.example`](.env.example) also
lists the optional settings, such as the player port, the edge domain, where management is published, and the images.
The `chunk` network has a fixed name, so one engine runs one install.

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

The dashboard is at `CHUNK_PUBLIC_URL`, `http://localhost:8080` by default; sign in with the operator token from `.env`.
The [`chunk` CLI](../../crates/chunk-cli/README.md) logs in through it. Scripts can call the `chunk.management.v1` API
over Connect JSON with the operator token as a bearer token.

To reach management from elsewhere, put a TLS reverse proxy in front, and set `CHUNK_PUBLIC_URL` to its URL and
`CHUNK_MANAGEMENT_PUBLISH` to where the proxy reaches it.

## Deploying

Back in the repository root, build the CLI and put it on your `PATH`, then log in, create a project and an environment,
and deploy a project to it, here the [local example](../../examples/local/README.md):

```sh
just toolchain && export PATH="$PWD/target/debug:$PATH"
chunk auth login --url http://localhost:8080  # CHUNK_PUBLIC_URL
chunk projects create my-server
chunk environments create prod
chunk deploy examples/local --env prod
```

`chunk auth login` prints a link to the dashboard; open it, signed in with the operator token, and approve the login.
`chunk deploy` builds a release, uploads it unless management already holds it, deploys it and waits until it is active,
then prints the environment's hostname. Run it again to deploy a new release; the dashboard can roll back to an earlier
one.

Players join at that hostname and the player port, for example `env-<id>.localhost:25565` from this host with the
default domain. The edge routes by hostname, so a bare IP address or `localhost` reaches no environment.

To see what is running:

```sh
chunk environments            # state, players and hostname
chunk deployments --env prod  # recent deployments
chunk apps --env prod         # the active release's apps and session types
```

Environments don't send their logs to management yet, so `chunk logs` prints nothing here. Read them from the engine
instead: `docker ps --filter label=chunk.environment` lists the machines, and `docker logs` shows each one's.

`just managed-smoke`, from the repository root, deploys the local example to a throwaway install like this one, plays it
with offline test players, and removes everything it created. It refuses to run while another install uses the engine.

## Stopping

Environments' machines are not part of the Compose project: they keep running without management, and they keep the
`chunk` network in use. To take the install down, delete every environment first. Neither the CLI nor the dashboard
deletes environments yet, so call the API, from `deploy/compose` again:

```sh
cd deploy/compose  # from the repository root
url=http://localhost:8080  # CHUNK_PUBLIC_URL
token=$(sed -n 's/^CHUNK_OPERATOR_TOKEN=//p' .env)
environments="ENVIRONMENT_ID ..."  # every ID `chunk environments` lists
call() {
  curl -sS -o /dev/null -w '%{http_code}\n' "$url/chunk.management.v1.ProjectService/$1" \
    -H "authorization: Bearer $token" -H 'content-type: application/json' -d "{\"environmentId\":\"$2\"}"
}
for environment in $environments; do
  call DeleteEnvironment "$environment"
  until [ "$(call GetEnvironment "$environment")" = 404 ]; do sleep 2; done
done
```

Once management reports them gone, stop it so it starts nothing new, remove anything a provider call still in flight
left behind, and take the stack down:

```sh
docker compose stop
for environment in $environments; do
  for container in $(docker ps -aq --filter "label=chunk.environment=$environment"); do docker rm -fv "$container"; done
  for volume in $(docker volume ls -q --filter "label=chunk.environment=$environment"); do docker volume rm "$volume"; done
done
docker compose down
```

With Podman, write `podman` for `docker`. Add `-v` to `compose down` only once no environment remains: it deletes the
database and stored releases, including the install ID that management uses to recognize and clean up its own machines.
