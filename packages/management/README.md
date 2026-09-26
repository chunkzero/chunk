# @chunkzero/management

The single-tenant management service: serves `chunk.management.v1` (see `proto/chunk/management/v1`) with Connect on
Bun, backed by Postgres. Migrations in `migrations/` apply on start.

## Running

```sh
DATABASE_URL=postgres://user:password@localhost:5432/chunk \
CHUNK_SECRET_KEY=$(head -c 32 /dev/urandom | base64) \
CHUNK_OPERATOR_TOKEN=chunk_$(head -c 32 /dev/urandom | base64 | tr -d '/+=') \
bun src/main.ts
```

| Variable                             | Default                     | Purpose                                                                                            |
| ------------------------------------ | --------------------------- | -------------------------------------------------------------------------------------------------- |
| `DATABASE_URL`                       | required                    | Postgres connection URL.                                                                           |
| `CHUNK_SECRET_KEY`                   | required                    | 32 bytes, base64. Encrypts secrets and signs upload URLs; keep it stable.                          |
| `CHUNK_OPERATOR_TOKEN`               | unset                       | An API token for the operator, recorded on start. At least 32 characters.                          |
| `CHUNK_PUBLIC_URL`                   | `http://localhost:$PORT`    | How clients reach this service; used in upload and login URLs.                                     |
| `HOST` / `PORT`                      | `0.0.0.0` / `8080`          | Listen address.                                                                                    |
| `CHUNK_DATA_DIR`                     | `data`                      | Release archives are stored under `releases/` here.                                                |
| `CHUNK_DASHBOARD_DIR`                | unset                       | The dashboard's build (`pnpm build:dashboard` writes `packages/dashboard/dist`), served at `/`.    |
| `CHUNK_MAX_RELEASE_EXPANDED_BYTES`   | `8589934592`                | How far a release archive may expand while it is verified.                                         |
| `CHUNK_MAX_RELEASE_ENTRIES`          | `100000`                    | How many tar entries a release archive may hold.                                                   |
| `CHUNK_EDGE_DOMAIN`                  | unset                       | Environments get `env-<id>.<domain>` hostnames; point `*.<domain>` at the edge.                    |
| `CHUNK_EDGE_PORT`                    | `25565`                     | The edge's player port, used in custom domains' SRV records.                                       |
| `CHUNK_EDGE_TOKEN`                   | unset                       | The token edges call `EdgeService` with, recorded on start. At least 32 characters.                |
| `CHUNK_LOG_STORE_BUCKET`             | unset                       | An S3-compatible bucket environments replicate their logs to. Unset turns replication off.         |
| `CHUNK_LOG_STORE_ENDPOINT`           | required with a bucket      | The bucket's endpoint URL.                                                                         |
| `CHUNK_LOG_STORE_REGION`             | `us-east-1`                 | The bucket's region, also used to sign STS requests.                                               |
| `CHUNK_LOG_STORE_PREFIX`             | `environments/`             | Each environment replicates below `<prefix><environment ID>/`.                                     |
| `CHUNK_LOG_STORE_ACCESS_KEY_ID`      | required with a bucket      | The operator's credentials, used to request each environment's own.                                |
| `CHUNK_LOG_STORE_SECRET_ACCESS_KEY`  | required with a bucket      | The secret for `CHUNK_LOG_STORE_ACCESS_KEY_ID`.                                                    |
| `CHUNK_LOG_STORE_ROLE_ARN`           | required with a bucket      | The role STS AssumeRole issues environment credentials under.                                      |
| `CHUNK_LOG_STORE_STS_ENDPOINT`       | `$CHUNK_LOG_STORE_ENDPOINT` | The STS endpoint; MinIO serves it on the S3 endpoint, AWS at `https://sts.<region>.amazonaws.com`. |
| `CHUNK_LOG_STORE_CREDENTIAL_SECONDS` | `3600`                      | How long issued environment credentials last.                                                      |
| `CHUNK_LOG_STORE_SHARED_CREDENTIALS` | unset                       | `1` hands every environment the operator's credentials instead; see below.                         |
| `CHUNK_ENVIRONMENT_IMAGE`            | unset                       | The environment image. Unset, no machines are provisioned.                                         |
| `DOCKER_HOST`                        | `/var/run/docker.sock`      | The Docker or Podman API socket machines run on, as `unix://<path>`.                               |
| `CHUNK_MACHINE_NETWORK`              | `chunk`                     | The container network machines join; created when missing.                                         |
| `CHUNK_MACHINE_MANAGEMENT_URL`       | `$CHUNK_PUBLIC_URL`         | How machines reach this service.                                                                   |
| `CHUNK_CORE_MEMORY_MIB`              | `1024`                      | Memory for each environment's core machine; CPUs are 1 per 2 GiB, at least 1.                      |
| `CHUNK_CORE_PORT`                    | `7070`                      | The port extra machines reach core on.                                                             |

Clients call `POST $CHUNK_PUBLIC_URL/chunk.management.v1.<Service>/<Method>` with `Authorization: Bearer <token>`, using
the Connect protocol (`application/proto` or `application/json`) or gRPC-Web over HTTP/1.1. Bun does not serve HTTP/2,
so plain gRPC clients do not work.

## Releases

Management is a registry for release archives. A READY release means the archive is stored and intact: it matches the
declared size and SHA-256, it is a well-formed tar within the expansion and entry limits, and `release.json` names the
release and declares the apps, sessions and machine profiles management reads. Management does not validate the backend
contract or the rest of the release. The environment decides whether a release is deployable when it loads it.

## Development

```sh
pnpm --filter @chunkzero/management generate   # regenerate src/gen from proto/ (needs buf from mise)
pnpm --filter @chunkzero/management typecheck
podman run -d --rm --name chunk-test-postgres -e POSTGRES_PASSWORD=test -p 127.0.0.1:55432:5432 \
  docker.io/library/postgres:17
TEST_DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres pnpm --filter @chunkzero/management test
```

Tests that need Postgres skip when `TEST_DATABASE_URL` is unset. Each test file uses its own schema. The provider tests
use `DOCKER_HOST`, or rootless Podman's socket, and skip when neither exists. The STS log store test runs against MinIO
when `TEST_MINIO_URL` is set, for example
`podman run --rm -p 127.0.0.1:59000:9000 -e MINIO_ROOT_USER=chunkroot -e MINIO_ROOT_PASSWORD=chunkrootsecret cgr.dev/chainguard/minio server /data`
with `TEST_MINIO_URL=http://127.0.0.1:59000`.

## Machines

With `CHUNK_ENVIRONMENT_IMAGE` set, a reconciler gives each environment with a deployment a core machine, runs the
machines `EnvironmentService.EnsureCapacity` asks for, suspends environments on a current idle report, and resumes them
for accepted wakes and due wake alarms. Every machine runs the environment image, and `CHUNK_SERVICES` selects what it
runs: `core,gateway,exec` on the core machine, and `jvm`, `gateway` or `exec` on an extra machine.

Core gets `CHUNK_MANAGEMENT_URL`, `CHUNK_ENVIRONMENT_ID` and its `CHUNK_ENVIRONMENT_TOKEN`. Extra machines never call
this service. They get `CHUNK_CORE_ADDRESS` and a `CHUNK_JOIN_TOKEN` valid for 15 minutes, plus the request's
`CHUNK_CAPACITY_REQUEST_ID`, `CHUNK_RELEASE_ID`, `CHUNK_APP_ID` and `CHUNK_MACHINE_PROFILE`. A join token is
`chunkjoin.v1.<claims>.<mac>`: base64url JSON claims (`environment_id`, `request_id`, `workload`, `expire_time` in Unix
seconds) and the base64url HMAC-SHA256 of everything before the last dot, keyed by the SHA-256 of core's environment
token. See `verifyJoinToken` in `src/environments/machines.ts`.

Every container and volume carries ownership labels with this install's ID (from the `installation` table), the
environment and the capacity request. The provider refuses to adopt, start, stop or remove anything under a name it uses
that lacks them. Core is restarted by the engine. Extra machines are stateless and are not: the reconciler replaces one
that exits or disappears, under the same request, with a fresh join token.

## Log replication

Each environment gets credentials for its own prefix, `<prefix><environment ID>/`, through `Attach`: temporary ones from
STS AssumeRole with an inline session policy that allows only that prefix, refreshed before they expire. This works with
AWS S3 and MinIO. `CHUNK_LOG_STORE_SHARED_CREDENTIALS=1` instead hands every environment the operator's static
credentials. That trusts every environment's code with every other environment's logs, so use it only when all
environments run code you trust.
