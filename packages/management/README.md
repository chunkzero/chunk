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

| Variable               | Default                  | Purpose                                                                         |
| ---------------------- | ------------------------ | ------------------------------------------------------------------------------- |
| `DATABASE_URL`         | required                 | Postgres connection URL.                                                        |
| `CHUNK_SECRET_KEY`     | required                 | 32 bytes, base64. Encrypts secrets and signs upload URLs; keep it stable.       |
| `CHUNK_OPERATOR_TOKEN` | unset                    | An API token for the operator, recorded on start. At least 32 characters.       |
| `CHUNK_PUBLIC_URL`     | `http://localhost:$PORT` | How clients reach this service; used in upload and login URLs.                  |
| `HOST` / `PORT`        | `0.0.0.0` / `8080`       | Listen address.                                                                 |
| `CHUNK_DATA_DIR`       | `data`                   | Release archives are stored under `releases/` here.                             |
| `CHUNK_EDGE_DOMAIN`    | unset                    | Environments get `env-<id>.<domain>` hostnames; point `*.<domain>` at the edge. |
| `CHUNK_EDGE_PORT`      | `25565`                  | The edge's player port, used in custom domains' SRV records.                    |

Clients call `POST $CHUNK_PUBLIC_URL/chunk.management.v1.<Service>/<Method>` with `Authorization: Bearer <token>`, using
the Connect protocol (`application/proto` or `application/json`) or gRPC-Web over HTTP/1.1. Bun does not serve HTTP/2,
so plain gRPC clients do not work.

## Development

```sh
pnpm --filter @chunkzero/management generate   # regenerate src/gen from proto/ (needs buf from mise)
pnpm --filter @chunkzero/management typecheck
podman run -d --rm --name chunk-test-postgres -e POSTGRES_PASSWORD=test -p 127.0.0.1:55432:5432 \
  docker.io/library/postgres:17
TEST_DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres pnpm --filter @chunkzero/management test
```

Tests that need Postgres skip when `TEST_DATABASE_URL` is unset. Each test file uses its own schema.
