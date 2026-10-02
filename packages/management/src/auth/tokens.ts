import { create } from "@bufbuild/protobuf";
import { and, eq, isNull, sql } from "drizzle-orm";

import { newId, randomToken, sha256 } from "../crypto.ts";
import { type Db, fetchRows } from "../db.ts";
import { type Token, TokenSchema } from "../gen/chunk/management/v1/auth_pb.ts";
import type { Authenticator, Principal } from "../rpc/caller.ts";
import { timestamp } from "../rpc/validate.ts";
import { apiTokens } from "../schema.ts";

/** The single-tenant install's one person. */
export const operator: Principal = { id: "operator", displayName: "Operator" };

export const tokenColumns = {
  id: apiTokens.id,
  principal_id: apiTokens.principal_id,
  name: apiTokens.name,
  project_id: apiTokens.project_id,
  create_time: apiTokens.create_time,
  expire_time: apiTokens.expire_time,
};

export type TokenRow = Pick<typeof apiTokens.$inferSelect, keyof typeof tokenColumns>;

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
  const [row] = await db
    .insert(apiTokens)
    .values({
      id: newId("tok"),
      principal_id: token.principalId,
      name: token.name,
      project_id: token.projectId ?? null,
      secret_hash: sha256(secret),
      expire_time: token.expireTime ?? null,
      renews: token.renews ?? false,
    })
    .returning(tokenColumns);
  if (!row) throw new Error("token insert returned no row");
  return { row, secret };
}

export async function findToken(db: Db, id: string): Promise<TokenRow | undefined> {
  const [row] = await db.select(tokenColumns).from(apiTokens).where(eq(apiTokens.id, id));
  return row;
}

/** Records the operator's configured token once; revoking it sticks until the configured value changes. */
export async function ensureOperatorToken(db: Db, secret: string): Promise<void> {
  await db
    .insert(apiTokens)
    .values({ id: newId("tok"), principal_id: operator.id, name: "CHUNK_OPERATOR_TOKEN", secret_hash: sha256(secret) })
    .onConflictDoNothing({ target: apiTokens.secret_hash });
}

/** Issues the token an environment's core uses, revoking the environment's earlier ones. */
export async function issueEnvironmentToken(db: Db, environmentId: string): Promise<string> {
  await db
    .update(apiTokens)
    .set({ revoke_time: sql`now()` })
    .where(
      and(
        eq(apiTokens.environment_id, environmentId),
        eq(apiTokens.kind, "environment"),
        isNull(apiTokens.revoke_time),
      ),
    );
  const secret = `${secretPrefix}${randomToken()}`;
  await db.insert(apiTokens).values({
    id: newId("tok"),
    principal_id: "",
    name: "environment",
    kind: "environment",
    environment_id: environmentId,
    secret_hash: sha256(secret),
  });
  return secret;
}

/** Records the configured edge token once, like `ensureOperatorToken`. */
export async function ensureEdgeToken(db: Db, secret: string): Promise<void> {
  await db
    .insert(apiTokens)
    .values({ id: newId("tok"), principal_id: "", name: "CHUNK_EDGE_TOKEN", kind: "edge", secret_hash: sha256(secret) })
    .onConflictDoNothing({ target: apiTokens.secret_hash });
}

export function tokenAuthenticator(db: Db): Authenticator {
  return {
    async authenticate(bearer) {
      // A renewing token is written at most about once per `renewalGraceMs`.
      const [row] = await fetchRows<{
        id: string;
        kind: string;
        principal_id: string;
        project_id: string | null;
        environment_id: string | null;
      }>(
        db,
        sql`
        with found as (
          select id, kind, principal_id, project_id, environment_id from api_tokens
          where secret_hash = ${sha256(bearer)} and revoke_time is null and (expire_time is null or expire_time > now())
        ), renewed as (
          update api_tokens
          set expire_time = now() + ${renewingLifetimeMs} * interval '1 millisecond'
          where id in (select id from found) and renews
            and expire_time < now() + ${renewingLifetimeMs - renewalGraceMs} * interval '1 millisecond'
        )
        select id, kind, principal_id, project_id, environment_id from found`,
      );
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
