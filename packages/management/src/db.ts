import { readdir } from "node:fs/promises";
import { join } from "node:path";

import postgres from "postgres";

export type Sql = postgres.Sql<{ bigint: bigint }>;
/** A connection or a transaction; anything that runs queries. */
export type Db = postgres.ISql<{ bigint: bigint }>;

const migrations = join(import.meta.dir, "..", "migrations");

export function connect(url: string, options: { searchPath?: string } = {}): Sql {
  return postgres(url, {
    types: { bigint: postgres.BigInt },
    onnotice: () => {},
    ...(options.searchPath ? { connection: { search_path: options.searchPath } } : {}),
  });
}

/** Applies the `migrations/*.sql` files not yet recorded, in name order, in one transaction. */
export async function migrate(sql: Sql): Promise<void> {
  const names = (await readdir(migrations)).filter((name) => name.endsWith(".sql")).sort();
  await sql.begin(async (tx) => {
    await tx`select pg_advisory_xact_lock(hashtext('chunk.management.migrate'))`;
    await tx`create table if not exists schema_migrations (
      name text primary key,
      apply_time timestamptz not null default now()
    )`;
    const applied = new Set((await tx<{ name: string }[]>`select name from schema_migrations`).map((row) => row.name));
    for (const name of names.filter((name) => !applied.has(name))) {
      await tx.file(join(migrations, name));
      await tx`insert into schema_migrations (name) values (${name})`;
    }
  });
}

export function isUniqueViolation(error: unknown): boolean {
  return error instanceof postgres.PostgresError && error.code === "23505";
}
