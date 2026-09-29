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

/**
 * Applies the `migrations/*.sql` files not yet recorded, in name order, then those in `extensionDir`, in one
 * transaction. An extension's migrations are recorded as `extension/<name>`, so their names never collide with chunk's.
 */
export async function migrate(sql: Sql, extensionDir?: string): Promise<void> {
  const sources = [{ directory: migrations, prefix: "" }];
  if (extensionDir !== undefined) sources.push({ directory: extensionDir, prefix: "extension/" });
  await sql.begin(async (tx) => {
    await tx`select pg_advisory_xact_lock(hashtext('chunk.management.migrate'))`;
    await tx`create table if not exists schema_migrations (
      name text primary key,
      apply_time timestamptz not null default now()
    )`;
    const applied = new Set((await tx<{ name: string }[]>`select name from schema_migrations`).map((row) => row.name));
    for (const { directory, prefix } of sources) {
      const names = (await readdir(directory)).filter((name) => name.endsWith(".sql")).sort();
      for (const name of names.filter((name) => !applied.has(prefix + name))) {
        await tx.file(join(directory, name));
        await tx`insert into schema_migrations (name) values (${prefix + name})`;
      }
    }
  });
}

/**
 * Advisory lock `key`, held by a session of its own: a client with one connection that is never retired, so the lock is
 * freed only when that connection is lost, and the next `hold` reconnects and tries again.
 */
export function advisoryLock(url: string, key: number) {
  const session = postgres(url, { max: 1, idle_timeout: 0, max_lifetime: null, onnotice: () => {} });
  return {
    /** Whether the session holds the lock, taking it when no other session does. */
    async hold(): Promise<boolean> {
      const [row] = await session<{ held: boolean }[]>`
        select case when exists (
          select 1 from pg_locks
          where locktype = 'advisory' and classid = 0 and objid = ${key} and objsubid = 1 and pid = pg_backend_pid()
        ) then true else pg_try_advisory_lock(${key}) end as held`;
      return row?.held === true;
    },
    close: () => session.end({ timeout: 5 }),
  };
}

export function isUniqueViolation(error: unknown): boolean {
  return error instanceof postgres.PostgresError && error.code === "23505";
}
