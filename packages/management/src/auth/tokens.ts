import { create } from "@bufbuild/protobuf";

import { newId, randomToken, sha256 } from "../crypto.ts";
import type { Db } from "../db.ts";
import { type Token, TokenSchema } from "../gen/chunk/management/v1/auth_pb.ts";
import type { Authenticator, Principal } from "../rpc/caller.ts";
import { timestamp } from "../rpc/validate.ts";

/** The single-tenant install's one person. */
export const operator: Principal = { id: "operator", displayName: "Operator" };

export interface TokenRow {
  id: string;
  principal_id: string;
  name: string;
  project_id: string | null;
  create_time: Date;
  expire_time: Date | null;
}

const secretPrefix = "chunk_";

/** How long a renewing token stays valid after its last use. */
export const renewingLifetimeMs = 30 * 24 * 60 * 60 * 1000;
const renewalGraceMs = 24 * 60 * 60 * 1000;

export function toToken(row: TokenRow): Token {
  return create(TokenSchema, {
    id: row.id,
    name: row.name,
    projectId: row.project_id ?? "",
    createTime: timestamp(row.create_time),
    expireTime: timestamp(row.expire_time),
  });
}

export async function issueToken(
  db: Db,
  token: {
    principalId: string;
    name: string;
    projectId: string | undefined;
    expireTime: Date | undefined;
    renews?: boolean;
  },
): Promise<{ row: TokenRow; secret: string }> {
  const secret = `${secretPrefix}${randomToken()}`;
  const [row] = await db<TokenRow[]>`
    insert into api_tokens (id, principal_id, name, project_id, secret_hash, expire_time, renews)
    values (${newId("tok")}, ${token.principalId}, ${token.name}, ${token.projectId ?? null}, ${sha256(secret)},
      ${token.expireTime ?? null}, ${token.renews ?? false})
    returning id, principal_id, name, project_id, create_time, expire_time`;
  if (!row) throw new Error("token insert returned no row");
  return { row, secret };
}

export async function findToken(db: Db, id: string): Promise<TokenRow | undefined> {
  const [row] = await db<TokenRow[]>`
    select id, principal_id, name, project_id, create_time, expire_time from api_tokens where id = ${id}`;
  return row;
}

/** Records the operator's configured token once; revoking it sticks until the configured value changes. */
export async function ensureOperatorToken(db: Db, secret: string): Promise<void> {
  await db`
    insert into api_tokens (id, principal_id, name, secret_hash)
    values (${newId("tok")}, ${operator.id}, 'CHUNK_OPERATOR_TOKEN', ${sha256(secret)})
    on conflict (secret_hash) do nothing`;
}

/** Issues the token an environment's core uses, revoking the environment's earlier ones. */
export async function issueEnvironmentToken(db: Db, environmentId: string): Promise<string> {
  await db`
    update api_tokens set revoke_time = now()
    where environment_id = ${environmentId} and kind = 'environment' and revoke_time is null`;
  const secret = `${secretPrefix}${randomToken()}`;
  await db`
    insert into api_tokens (id, principal_id, name, kind, environment_id, secret_hash)
    values (${newId("tok")}, '', 'environment', 'environment', ${environmentId}, ${sha256(secret)})`;
  return secret;
}

/** Records the configured edge token once, like `ensureOperatorToken`. */
export async function ensureEdgeToken(db: Db, secret: string): Promise<void> {
  await db`
    insert into api_tokens (id, principal_id, name, kind, secret_hash)
    values (${newId("tok")}, '', 'CHUNK_EDGE_TOKEN', 'edge', ${sha256(secret)})
    on conflict (secret_hash) do nothing`;
}

export function tokenAuthenticator(db: Db): Authenticator {
  return {
    async authenticate(bearer) {
      // A renewing token is written at most about once per `renewalGraceMs`.
      const [row] = await db<
        { id: string; kind: string; principal_id: string; project_id: string | null; environment_id: string | null }[]
      >`
        with found as (
          select id, kind, principal_id, project_id, environment_id from api_tokens
          where secret_hash = ${sha256(bearer)} and revoke_time is null and (expire_time is null or expire_time > now())
        ), renewed as (
          update api_tokens
          set expire_time = now() + ${renewingLifetimeMs} * interval '1 millisecond'
          where id in (select id from found) and renews
            and expire_time < now() + ${renewingLifetimeMs - renewalGraceMs} * interval '1 millisecond'
        )
        select id, kind, principal_id, project_id, environment_id from found`;
      if (!row) return undefined;
      if (row.kind === "environment" && row.environment_id) {
        return { kind: "environment", environmentId: row.environment_id, tokenId: row.id };
      }
      if (row.kind === "edge") return { kind: "edge", tokenId: row.id };
      if (row.kind !== "person" || row.principal_id !== operator.id) return undefined;
      return {
        kind: "person",
        caller: { principal: operator, tokenId: row.id, projectId: row.project_id ?? undefined },
      };
    },
  };
}
