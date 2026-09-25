import { create } from "@bufbuild/protobuf";
import type { ServiceImpl } from "@connectrpc/connect";

import type { Deps } from "../deps.ts";
import { SecretSchema, SecretService, SetSecretResponseSchema } from "../gen/chunk/management/v1/secrets_pb.ts";
import { loadEnvironment } from "../projects/store.ts";
import { callerOf } from "../rpc/caller.ts";
import { idempotent } from "../rpc/idempotency.ts";
import { invalid, page, pageOf, timestamp } from "../rpc/validate.ts";

interface SecretRow {
  environment_id: string;
  name: string;
  version: bigint;
  update_time: Date;
}

const namePattern = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
const maxValueBytes = 64 * 1024;

/** Binds a stored ciphertext to its environment and name. */
export function secretContext(environmentId: string, name: string): string {
  return `secret/${environmentId}/${name}`;
}

export function secretService({ sql, keys }: Deps): Partial<ServiceImpl<typeof SecretService>> {
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
      if (request.value.byteLength > maxValueBytes) throw invalid(`value must be at most ${maxValueBytes} bytes`);
      return idempotent({ sql, keys, caller, method: SecretService.method.setSecret, request }, async (tx) => {
        const environment = await loadEnvironment(tx, caller, request.environmentId);
        const ciphertext = await keys.cipher.seal(request.value, secretContext(environment.id, name));
        const [row] = await tx<SecretRow[]>`
          insert into secrets (environment_id, name, version, ciphertext)
          values (${environment.id}, ${name}, 1, ${ciphertext})
          on conflict (environment_id, name) do update
            set version = secrets.version + 1, ciphertext = excluded.ciphertext, update_time = now()
          returning environment_id, name, version, update_time`;
        return create(SetSecretResponseSchema, { secret: row && toSecret(row) });
      });
    },

    async listSecrets(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
      const p = page(request);
      const rows = await sql<SecretRow[]>`
        select environment_id, name, version, update_time from secrets
        where environment_id = ${environment.id}
          and ciphertext is not null
          ${p.after === undefined ? sql`` : sql`and name > ${p.after}`}
        order by name
        limit ${p.size + 1}`;
      const { items, nextPageToken } = pageOf(rows, p, (row) => row.name);
      return { secrets: items.map(toSecret), nextPageToken };
    },

    async deleteSecret(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
      await sql`
        update secrets set ciphertext = null, update_time = now()
        where environment_id = ${environment.id} and name = ${secretName(request.name)} and ciphertext is not null`;
      return {};
    },
  };
}

function secretName(name: string): string {
  if (!namePattern.test(name)) {
    throw invalid("name must be 1-128 letters, digits or underscores, not starting with a digit");
  }
  return name;
}
