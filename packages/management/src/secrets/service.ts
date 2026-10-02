import { create } from "@bufbuild/protobuf";
import type { ServiceImpl } from "@connectrpc/connect";
import { and, count, eq, gt, isNotNull, ne, sql } from "drizzle-orm";

import type { Keys } from "../crypto.ts";
import type { Db } from "../db.ts";
import type { Deps } from "../deps.ts";
import { advanceRevision } from "../environments/store.ts";
import { SecretSchema, SecretService, SetSecretResponseSchema } from "../gen/chunk/management/v1/secrets_pb.ts";
import { loadEnvironment } from "../projects/store.ts";
import { callerOf } from "../rpc/caller.ts";
import { idempotent } from "../rpc/idempotency.ts";
import { invalid, page, pageOf, timestamp } from "../rpc/validate.ts";
import { environments, secrets } from "../schema.ts";

const secretColumns = {
  environment_id: secrets.environment_id,
  name: secrets.name,
  version: secrets.version,
  update_time: secrets.update_time,
};

type SecretRow = Pick<typeof secrets.$inferSelect, keyof typeof secretColumns>;

const namePattern = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
const maxValueBytes = 64 * 1024;
const maxSecrets = 256;
const utf8 = new TextDecoder("utf-8", { fatal: true });

/** Binds a stored ciphertext to its environment and name. */
export function secretContext(environmentId: string, name: string): string {
  return `secret/${environmentId}/${name}`;
}

/** Copies every secret `from` holds to `to`, which holds none yet, each as its first version. */
export async function copySecrets(db: Db, keys: Keys, from: string, to: string): Promise<void> {
  const stored = await db
    .select({ name: secrets.name, ciphertext: sql<Uint8Array>`${secrets.ciphertext}` })
    .from(secrets)
    .where(and(eq(secrets.environment_id, from), isNotNull(secrets.ciphertext)));
  for (const { name, ciphertext } of stored) {
    const value = await keys.cipher.open(ciphertext, secretContext(from, name));
    const sealed = await keys.cipher.seal(value, secretContext(to, name));
    await db.insert(secrets).values({ environment_id: to, name, version: 1n, ciphertext: sealed });
  }
}

export function secretService({ db, keys }: Deps): Partial<ServiceImpl<typeof SecretService>> {
  const toSecret = (row: SecretRow) =>
    create(SecretSchema, {
      environmentId: row.environment_id,
      name: row.name,
      version: row.version,
      updateTime: timestamp(row.update_time),
    });

  return {
    async setSecret(request, context) {
      const caller = callerOf(context);
      const name = secretName(request.name);
      secretValue(request.value);
      return idempotent({ db, keys, caller, method: SecretService.method.setSecret, request }, async (tx) => {
        const environment = await loadEnvironment(tx, caller, request.environmentId);
        // Serializes concurrent sets, so the limit holds.
        await tx
          .select({ one: sql`1` })
          .from(environments)
          .where(eq(environments.id, environment.id))
          .for("update");
        const [held] = await tx
          .select({ others: count() })
          .from(secrets)
          .where(
            and(eq(secrets.environment_id, environment.id), ne(secrets.name, name), isNotNull(secrets.ciphertext)),
          );
        if ((held?.others ?? 0) >= maxSecrets) throw invalid(`an environment holds at most ${maxSecrets} secrets`);
        const ciphertext = await keys.cipher.seal(request.value, secretContext(environment.id, name));
        const [row] = await tx
          .insert(secrets)
          .values({ environment_id: environment.id, name, version: 1n, ciphertext })
          .onConflictDoUpdate({
            target: [secrets.environment_id, secrets.name],
            set: {
              version: sql`${secrets.version} + 1`,
              ciphertext: sql`excluded.ciphertext`,
              update_time: sql`now()`,
            },
          })
          .returning(secretColumns);
        await advanceRevision(tx, environment.id);
        return {
          response: create(SetSecretResponseSchema, { secret: row && toSecret(row) }),
          projectId: environment.project_id,
        };
      });
    },

    async listSecrets(request, context) {
      const environment = await loadEnvironment(db, callerOf(context), request.environmentId);
      const p = page(request);
      const rows = await db
        .select(secretColumns)
        .from(secrets)
        .where(
          and(
            eq(secrets.environment_id, environment.id),
            isNotNull(secrets.ciphertext),
            p.after === undefined ? undefined : gt(secrets.name, p.after),
          ),
        )
        .orderBy(secrets.name)
        .limit(p.size + 1);
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.name);
      return { secrets: items.map(toSecret), nextPageToken };
    },

    async deleteSecret(request, context) {
      const environment = await loadEnvironment(db, callerOf(context), request.environmentId);
      const deleted = await db
        .update(secrets)
        .set({ ciphertext: null, update_time: sql`now()` })
        .where(
          and(
            eq(secrets.environment_id, environment.id),
            eq(secrets.name, secretName(request.name)),
            isNotNull(secrets.ciphertext),
          ),
        )
        .returning({ name: secrets.name });
      if (deleted.length > 0) await advanceRevision(db, environment.id);
      return {};
    },
  };
}

function secretValue(value: Uint8Array): void {
  if (value.byteLength === 0 || value.byteLength > maxValueBytes) {
    throw invalid(`value must be 1 to ${maxValueBytes} bytes`);
  }
  try {
    utf8.decode(value);
  } catch {
    throw invalid("value must be UTF-8 text");
  }
}

function secretName(name: string): string {
  if (!namePattern.test(name)) {
    throw invalid("name must be 1-128 letters, digits or underscores, not starting with a digit");
  }
  return name;
}
