import { join } from "node:path";

import { SQL } from "bun";
import type { SQLWrapper } from "drizzle-orm";
import { type BunSQLDatabase, type BunSQLQueryResultHKT, drizzle } from "drizzle-orm/bun-sql";
import { migrate as applyMigrations } from "drizzle-orm/bun-sql/migrator";
import { DrizzleQueryError } from "drizzle-orm/errors";
import type { PgDatabase } from "drizzle-orm/pg-core";

/** The service's connection pool; `$client` is the Bun.SQL client underneath. */
export type Database = BunSQLDatabase & { $client: SQL };
/** A database or a transaction; anything that runs queries. */
export type Db = PgDatabase<BunSQLQueryResultHKT>;

const migrations = join(import.meta.dir, "..", "migrations");
/** The schema the migrations tables live in, Drizzle's default. */
const migrationsSchema = "drizzle";
const migrateLock = "chunk.management.migrate";

/** The rows a raw statement returns, typed as `T`: Drizzle's Bun.SQL driver leaves `execute`'s rows untyped. */
export async function fetchRows<T>(db: Db, query: SQLWrapper): Promise<T[]> {
  return (await db.execute(query)) as T[];
}

/** Connects to Postgres, returning `bigint` columns as `bigint`. */
export function connect(url: string): Database {
  return drizzle({ client: new SQL(url, { bigint: true }) });
}

/**
 * Applies chunk's migrations not yet recorded, then those in `extensionFolder`: a drizzle-kit migrations folder (its
 * `meta/_journal.json` and SQL files) for an install's own tables. Each folder is applied in a transaction of its own
 * and recorded in a table of its own, `drizzle.chunk_migrations` and `drizzle.extension_migrations`; a folder's
 * migrations newer than the last one recorded are applied in journal order. Processes sharing the database migrate
 * one at a time.
 */
export async function migrate(db: Database, extensionFolder?: string): Promise<void> {
  const session = await db.$client.reserve();
  try {
    await session`select pg_advisory_lock(hashtext(${migrateLock}))`;
    const locked = drizzle({ client: session });
    await applyMigrations(locked, {
      migrationsFolder: migrations,
      migrationsSchema,
      migrationsTable: "chunk_migrations",
    });
    if (extensionFolder !== undefined) {
      await applyMigrations(locked, {
        migrationsFolder: extensionFolder,
        migrationsSchema,
        migrationsTable: "extension_migrations",
      });
    }
  } finally {
    try {
      await session`select pg_advisory_unlock(hashtext(${migrateLock}))`;
    } finally {
      session.release();
    }
  }
}

/**
 * Advisory lock `key`, held by a session of its own: a client with one connection that is never retired, so the lock is
 * freed only when that connection is lost, and the next `hold` reconnects and tries again.
 */
export function advisoryLock(url: string, key: number) {
  const session = new SQL(url, { max: 1, idleTimeout: 0, maxLifetime: 0 });
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
    close: () => session.close({ timeout: 5 }),
  };
}

export function isUniqueViolation(error: unknown): boolean {
  const cause = error instanceof DrizzleQueryError ? error.cause : error;
  return cause instanceof SQL.PostgresError && cause.errno === "23505";
}
