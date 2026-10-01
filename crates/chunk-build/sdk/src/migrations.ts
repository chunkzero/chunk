import { freeze } from "./validators.ts";

/**
 * Each expand migration's row types by ID: the previous snapshot's row (`old`), the new row (`row`), and the fields
 * the migration adds and removes. Codegen declares them from the snapshots in `server/migrations/meta/`.
 */
export interface Migrations {}

interface TableTypes {
  old: object;
  row: object;
  added: object;
  removed: object;
}
type Exact<R, A> = R & { [K in Exclude<keyof R, keyof A>]: never };
type Returned<F> = F extends (...args: never[]) => infer R ? R : never;
type Transforms<S> = {
  [T in keyof S]: S[T] extends TableTypes
    ? { to(old: Readonly<S[T]["old"]>): S[T]["added"]; back?(row: Readonly<S[T]["row"]>): S[T]["removed"] }
    : never;
};
type Checked<M, S> = {
  [T in keyof M]: T extends keyof S
    ? S[T] extends TableTypes
      ? {
          to(old: Readonly<S[T]["old"]>): Exact<Returned<M[T] extends { to: infer F } ? F : never>, S[T]["added"]>;
          back?(
            row: Readonly<S[T]["row"]>,
          ): Exact<Returned<M[T] extends { back?: infer F } ? F : never>, S[T]["removed"]>;
        }
      : never
    : never;
};

type Transform = (row: Readonly<Record<string, unknown>>) => unknown;
export interface MigrationDefinition {
  readonly id: string;
  readonly tables: Readonly<Record<string, { readonly to: Transform; readonly back?: Transform }>>;
}

const definitions = new WeakSet<object>();

/**
 * Declares migration `id`'s transforms for each table it changes. `to` returns exactly the new fields from a row of
 * the previous snapshot; the optional `back` returns exactly the removed fields from a current row. Transforms are
 * pure functions of one row.
 */
export function defineMigration<const Id extends keyof Migrations & string, const M extends Transforms<Migrations[Id]>>(
  id: Id,
  tables: M & Checked<M, Migrations[Id]>,
): MigrationDefinition {
  const entries = Object.entries(tables as Record<string, { to?: unknown; back?: unknown } | undefined>);
  const checked = entries.map(([table, transforms]) => {
    const { to, back } = transforms ?? {};
    if (typeof to !== "function" || (back !== undefined && typeof back !== "function"))
      throw new Error(`Migration ${id} needs a to function for ${table}, and back must be a function when present`);
    return [table, back === undefined ? { to } : { to, back }];
  });
  const definition: MigrationDefinition = freeze({ id, tables: Object.fromEntries(checked) });
  definitions.add(definition);
  return definition;
}

export function isMigration(value: unknown): value is MigrationDefinition {
  return typeof value === "object" && value !== null && definitions.has(value);
}

/** Applies one migration's `to` or `back` to each row; the backend bundle exports this as `__chunk_migrate`. */
export function migrate(migrations: Record<string, MigrationDefinition>, args: unknown): unknown[] {
  const { migration, table, direction, rows } = args as Record<string, unknown>;
  const definition = typeof migration === "string" && Object.hasOwn(migrations, migration) && migrations[migration];
  const transforms =
    definition && typeof table === "string" && Object.hasOwn(definition.tables, table)
      ? definition.tables[table]
      : undefined;
  const transform = direction === "to" ? transforms?.to : direction === "back" ? transforms?.back : undefined;
  if (!transform || !Array.isArray(rows))
    throw new Error(`Migration ${String(migration)} has no ${String(direction)} transform for ${String(table)}`);
  return rows.map((row) => transform(freeze(row)));
}

/** Whether each migration's tables have a `back` transform. */
export function migrationBacks(
  migrations: Record<string, MigrationDefinition>,
): Record<string, Record<string, boolean>> {
  return Object.fromEntries(
    Object.entries(migrations).map(([id, definition]) => [
      id,
      Object.fromEntries(Object.entries(definition.tables).map(([table, { back }]) => [table, back !== undefined])),
    ]),
  );
}
