# @chunkzero/management

The self-hosted control plane. Management stores projects, environments and release archives, deploys releases, and
reconciles each environment's machines through a provider. It serves the `chunk.management.v1` API
([`proto/chunk/management/v1`](../../proto/chunk/management/v1)) and the [dashboard](../dashboard) on Bun, backed by
Postgres, and applies the migrations in `migrations/` on start. It ships a Docker/Podman provider;
[`deploy/compose`](../../deploy/compose/README.md) runs it with Postgres and the
[edge](../../crates/chunk-edge/README.md) on one host.

## Running

```sh
DATABASE_URL=postgres://user:password@localhost:5432/chunk \
CHUNK_SECRET_KEY=$(head -c 32 /dev/urandom | base64) \
CHUNK_OPERATOR_TOKEN=chunk_$(head -c 32 /dev/urandom | base64 | tr -d '/+=') \
bun src/main.ts
```

`just management-image` builds the `chunk-management:<workspace version>` image, dashboard included.

| Variable                               | Default                       | Meaning                                                                                                     |
| -------------------------------------- | ----------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `DATABASE_URL`                         | required                      | The Postgres connection URL.                                                                                |
| `CHUNK_SECRET_KEY`                     | required                      | 32 random bytes, base64. Encrypts stored secrets and signs upload URLs; keep it for the database's life.    |
| `CHUNK_OPERATOR_TOKEN`                 | unset                         | An API token for the operator, at least 32 characters, recorded on start.                                   |
| `CHUNK_PUBLIC_URL`                     | `http://localhost:$PORT`      | How clients reach this service; used in upload and login URLs.                                              |
| `HOST` / `PORT`                        | `0.0.0.0` / `8080`            | The listen address.                                                                                         |
| `CHUNK_DATA_DIR`                       | `data`                        | Release archives are stored under `releases/` here, unless a release bucket is set.                         |
| `CHUNK_DASHBOARD_DIR`                  | unset                         | The dashboard's build (`pnpm build:dashboard` writes `packages/dashboard/dist`), served at `/`.             |
| `CHUNK_MAX_RELEASE_EXPANDED_BYTES`     | `8589934592`                  | How far a release archive may expand while it is verified.                                                  |
| `CHUNK_MAX_RELEASE_ENTRIES`            | `100000`                      | How many entries a release archive may hold.                                                                |
| `CHUNK_EDGE_DOMAIN`                    | unset                         | New environments get the hostname `env-<24 hex digits>.<domain>`; point `*.<domain>` at the edge.           |
| `CHUNK_EDGE_PORT`                      | `25565`                       | The edge's player port, used in custom domains' SRV records and environments' join addresses.               |
| `CHUNK_EDGE_TOKEN`                     | unset                         | The token edges call `EdgeService` with, at least 32 characters, recorded on start.                         |
| `CHUNK_ENVIRONMENT_IMAGE`              | unset                         | The image core and gateway machines run. Unset, no machines run and nothing can be deployed.                |
| `CHUNK_JVM_IMAGE`                      | unset                         | The JVM runner image, containing `{java}`, which is replaced by the release's Java version.                 |
| `DOCKER_HOST`                          | `unix:///var/run/docker.sock` | The Docker or Podman API socket machines run on; only `unix://` works.                                      |
| `CHUNK_MACHINE_NETWORK`                | `chunk`                       | The container network machines join; created when missing.                                                  |
| `CHUNK_MACHINE_MANAGEMENT_URL`         | `$CHUNK_PUBLIC_URL`           | How machines reach this service, including release downloads.                                               |
| `CHUNK_CORE_MEMORY_MIB`                | `1024`                        | Memory for each core machine; it gets one CPU per 2 GiB, at least one.                                      |
| `CHUNK_CORE_PORT`                      | `7070`                        | The port core listens on for the environment's other machines.                                              |
| `CHUNK_MACHINE_TRUSTED_EDGES`          | unset                         | Edge IPs or CIDRs, comma-separated, that core and gateway machines accept PROXY headers from.               |
| `CHUNK_MACHINE_OFFLINE_LOGINS`         | unset                         | `1` lets anyone join under any name, unauthenticated. Insecure; for tests only.                             |
| `CHUNK_MACHINE_SUSPEND_AFTER_SECONDS`  | unset                         | Seconds idle before an environment sleeps; it wakes when a player logs in through the edge. Never if unset. |
| `CHUNK_RECONCILE_CONCURRENCY`          | `8`                           | How many environments the reconciler works on at once.                                                      |
| `CHUNK_PROVIDER_START_TIMEOUT_SECONDS` | `120`                         | How long the reconciler waits for a machine's create or start.                                              |
| `CHUNK_PROVIDER_TIMEOUT_SECONDS`       | `60`                          | How long it waits for every other provider call.                                                            |
| `CHUNK_CAPACITY_RETRY_SECONDS`         | `300`                         | How long a JVM or gateway request keeps retrying a provider with no room, or timing out, before it fails.   |
| `CHUNK_RELEASE_STORE_*`                | unset                         | A bucket for release archives; see [Release storage](#release-storage).                                     |
| `CHUNK_LOG_STORE_*`                    | unset                         | Log storage; see [Log storage](#log-storage).                                                               |

The machine and reconciler variables apply only when `CHUNK_ENVIRONMENT_IMAGE` is set.

## API

Clients call `POST $CHUNK_PUBLIC_URL/chunk.management.v1.<Service>/<Method>` with `Authorization: Bearer <token>`, over
the Connect protocol (`application/json` or `application/proto`) or gRPC-Web. Bun serves HTTP/1.1 only, so plain gRPC
clients don't work. `GET /healthz` answers `ok`.

| Service                                                                               | Callers                                                              |
| ------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| `ProjectService`, `DeploymentService`, `SecretService`, `DomainService`, `LogService` | The operator's tokens                                                |
| `AuthService`                                                                         | The operator's tokens; `StartLogin` and `PollLogin` need none        |
| `EnvironmentService`                                                                  | Each environment's core, with the token management gives its machine |
| `EdgeService`                                                                         | Edges, with `CHUNK_EDGE_TOKEN`                                       |

The operator signs in to the dashboard with a token. More tokens, optionally limited to one project or given an expiry,
come from `AuthService.CreateToken`, or from a device-style login: `StartLogin` returns a URL whose `/login` page the
operator approves in the dashboard, and `PollLogin` then hands the new token to the client that started it.

## Releases

A release is uploaded in three steps. `DeploymentService.UploadRelease` declares its ID, SHA-256 and size and returns an
`upload` target: a `PUT` URL valid for an hour, with its headers. Management serves it under `/releases/upload/`, signed
with `CHUNK_SECRET_KEY`, or presigns it for the release bucket (see [Release storage](#release-storage)). When the
project already holds the release intact, it returns no `upload`. After the `PUT`, `CompleteReleaseUpload` verifies the
archive and marks the release `RELEASE_STATE_READY`: it matches the declared size and digest, it is a well-formed gzip
tar within the expansion and entry limits, and its `release.json` names the release and declares its apps, sessions and
machine profiles. Management doesn't check the backend contract; the environment decides whether it can serve a release
when it loads it.

`Deploy` makes a ready release an environment's desired deployment, which the environment's core loads and reports as
`DEPLOYMENT_STATE_ACTIVE` or `DEPLOYMENT_STATE_FAILED`. `Deploy`, `Promote`, `Rollback` and JVM capacity requests refuse
a release with `FAILED_PRECONDITION` when there is no JVM image for it: `CHUNK_JVM_IMAGE` is unset, or `release.json`
has no integer `java_version` from 1 to 1000.

## Release storage

Release archives stay on management's disk under `CHUNK_DATA_DIR` unless `CHUNK_RELEASE_STORE_BUCKET` names an
S3-compatible bucket, such as AWS S3, MinIO or Cloudflare R2. Clients then upload to presigned URLs below
`<prefix>uploads/`, and machines download from presigned URLs below `<prefix>archives/`. A presigned URL can't bind an
upload to its digest, so `CompleteReleaseUpload` verifies the upload where it landed, then copies it to
`<prefix>archives/` through a second check of its size and digest, which refuses bytes replaced in between. Completing
reads the archive twice and writes it once, streaming. An upload that is never completed stays; a lifecycle rule can
expire `<prefix>uploads/`.

The connection settings fall back to the log store's, so one bucket can hold both; keep the two prefixes apart.

| Variable                                | Default                      | Meaning                                                                                     |
| --------------------------------------- | ---------------------------- | ------------------------------------------------------------------------------------------- |
| `CHUNK_RELEASE_STORE_BUCKET`            | unset                        | The S3-compatible bucket. Unset keeps archives on management's disk.                        |
| `CHUNK_RELEASE_STORE_ENDPOINT`          | the log store's              | The bucket's endpoint URL, as management and machines reach it; required without either.    |
| `CHUNK_RELEASE_STORE_PUBLIC_ENDPOINT`   | the endpoint                 | The endpoint clients upload to, when they reach the bucket at another address.              |
| `CHUNK_RELEASE_STORE_REGION`            | the log store's, `us-east-1` | The bucket's region.                                                                        |
| `CHUNK_RELEASE_STORE_PREFIX`            | `releases/`                  | The prefix uploads and archives go under.                                                   |
| `CHUNK_RELEASE_STORE_ACCESS_KEY_ID`     | the log store's              | Credentials that read, write and delete below the prefix; required without the log store's. |
| `CHUNK_RELEASE_STORE_SECRET_ACCESS_KEY` | the log store's              | The secret for the access key.                                                              |

## Machines

The reconciler gives each environment with a deployment a core machine running `CHUNK_ENVIRONMENT_IMAGE`
([`chunk-environment`](../../crates/chunk-environment/README.md)) with `CHUNK_SERVICES=core,gateway`. Core asks for JVM
machines with `EnvironmentService.EnsureCapacity`, which run `CHUNK_JVM_IMAGE`
([`chunk-jvm`](../../crates/chunk-jvm/README.md)); `EnsureCapacity` also accepts extra gateway machines, which run the
environment image with `CHUNK_SERVICES=gateway`.

Core gets `CHUNK_MANAGEMENT_URL`, `CHUNK_ENVIRONMENT_ID`, its `CHUNK_ENVIRONMENT_TOKEN` and
`CHUNK_CORE_BIND=[::]:$CHUNK_CORE_PORT`. The other machines never call management: they get `CHUNK_CORE_ENDPOINT`
(core's first IP address and `CHUNK_CORE_PORT`), `CHUNK_ENVIRONMENT_ID`, the credential core minted for them
(`CHUNK_GATEWAY_CREDENTIAL` or `CHUNK_JVM_CREDENTIAL`), and the request's `CHUNK_RELEASE_ID`, `CHUNK_APP_ID` and
`CHUNK_MACHINE_PROFILE`, which the JVM runner checks core's launch against. `CHUNK_MACHINE_TRUSTED_EDGES`,
`CHUNK_MACHINE_OFFLINE_LOGINS` and `CHUNK_MACHINE_SUSPEND_AFTER_SECONDS` reach core and gateway machines as
`CHUNK_TRUSTED_EDGES`, `CHUNK_OFFLINE_LOGINS` and (core only) `CHUNK_SUSPEND_AFTER_SECONDS`. Credentials are stored
sealed and never returned. Traffic between machines is plaintext, so they must share a private, encrypted network.

Every container and named volume the provider creates carries `chunk.install`, `chunk.environment`, `chunk.request` and
`chunk.workload` labels (anonymous volumes an image declares, such as the JVM runner's cache, go with their container),
and the provider refuses to touch anything under a name it uses that lacks this install's labels. Core restarts with the
engine. A gateway machine that exits is replaced under the same request and credential. A JVM machine boots at most
once: if it exits or disappears, its request fails and core asks for new capacity. A released request never gets a
running machine.

When core reports that it may be suspended, the reconciler suspends the environment's machines (the Docker provider
pauses them, keeping their memory). An accepted `EdgeService.Wake` or a due wake alarm resumes them. A JVM request whose
machine stopped instead of pausing fails, and core asks for new capacity once it wakes.

Several management processes may share a database; the one holding the reconciler's advisory lock leads, and a new
leader fences the old one's writes. The leader works on up to `CHUNK_RECONCILE_CONCURRENCY` environments at once, one
operation per environment, and tears down released machines alongside those operations, so core's suspension never waits
on its own releases. It gives up on a provider call after its timeout and observes the machines again on the next pass.

**Providers.** The package exports `start(config, extensions)` from `src/index.ts`. An install can plug in its own
provider, extra Connect services and migrations, its own authentication, and its own log store credential issuer through
`Extensions`, instead of the Docker/Podman provider `src/main.ts` uses.

## Log storage

With `CHUNK_LOG_STORE_BUCKET` set, management hands each environment credentials for its own prefix,
`<prefix><environment ID>/`, in `Attach`: temporary ones from STS `AssumeRole` with an inline policy that allows only
that prefix, refreshed before they expire. This works with AWS S3 and MinIO. Core replicates its log there, restores it
from there when it starts without its volume, and picks up the fresh credentials each desired state carries. Deleting an
environment removes its prefix, with the environment's own credentials, once its machines are gone; an install's own
issuer removes it through `LogStoreIssuer.deleteEnvironment`. Deletion removes current objects only, so leave bucket
versioning off, or add a lifecycle rule that expires noncurrent versions.

Forks need log storage too. `ListSnapshots` and `ForkEnvironment` list the source's prefix with read-only credentials
from `LogStoreIssuer.readGrant`, and `Attach` hands a fork's core the same kind for the source's prefix until that core
first attaches, by which time the fork's own prefix holds a snapshot. Until then, deleting the source keeps its prefix.
A fork starts without a deployment: its core restores it, and its first attach names the newest deployment the restored
data holds, whose release management then deploys to the fork under the `FORK` trigger.

| Variable                             | Default                | Meaning                                                                                            |
| ------------------------------------ | ---------------------- | -------------------------------------------------------------------------------------------------- |
| `CHUNK_LOG_STORE_BUCKET`             | unset                  | The S3-compatible bucket. Unset turns log storage off.                                             |
| `CHUNK_LOG_STORE_ENDPOINT`           | required with a bucket | The bucket's endpoint URL.                                                                         |
| `CHUNK_LOG_STORE_REGION`             | `us-east-1`            | The bucket's region, also used to sign STS requests.                                               |
| `CHUNK_LOG_STORE_PREFIX`             | `environments/`        | The prefix environments' prefixes go under.                                                        |
| `CHUNK_LOG_STORE_ACCESS_KEY_ID`      | required with a bucket | The operator's credentials, used to request each environment's own.                                |
| `CHUNK_LOG_STORE_SECRET_ACCESS_KEY`  | required with a bucket | The secret for the access key.                                                                     |
| `CHUNK_LOG_STORE_ROLE_ARN`           | required with a bucket | The role STS issues environment credentials under; not needed with shared credentials.             |
| `CHUNK_LOG_STORE_STS_ENDPOINT`       | the endpoint           | The STS endpoint; MinIO serves it on the S3 endpoint, AWS at `https://sts.<region>.amazonaws.com`. |
| `CHUNK_LOG_STORE_CREDENTIAL_SECONDS` | `3600`                 | How long issued credentials last.                                                                  |
| `CHUNK_LOG_STORE_SHARED_CREDENTIALS` | unset                  | `1` hands every environment the operator's credentials instead.                                    |

Shared credentials trust every environment's code with every other environment's logs; use them only when all
environments run code you trust.

## Development

```sh
pnpm --filter @chunkzero/management generate   # regenerate src/gen from proto/ (needs buf from mise)
pnpm --filter @chunkzero/management db:generate  # write a migration for changes to src/schema.ts
pnpm --filter @chunkzero/management typecheck
podman run -d --rm --name chunk-test-postgres -e POSTGRES_PASSWORD=test -p 127.0.0.1:55432:5432 \
  docker.io/library/postgres:17
TEST_DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres pnpm --filter @chunkzero/management test
```

`src/schema.ts` defines the tables, and `migrations/` holds the drizzle-kit migrations generated from it; review each
generated migration before committing it. Tests that need Postgres skip when `TEST_DATABASE_URL` is unset; each test
file uses its own database. The provider tests use `DOCKER_HOST`, or rootless Podman's socket, and skip when neither
exists. The STS and S3 release store tests run against MinIO when `TEST_MINIO_URL` is set, for example
`podman run --rm -p 127.0.0.1:59000:9000 -e MINIO_ROOT_USER=chunkroot -e MINIO_ROOT_PASSWORD=chunkrootsecret cgr.dev/chainguard/minio server /data`
with `TEST_MINIO_URL=http://127.0.0.1:59000`. `just managed-smoke` runs the whole self-hosted path end to end.
