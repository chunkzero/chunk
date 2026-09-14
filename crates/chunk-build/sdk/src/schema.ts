import { fields, freeze, identifier } from "./validators.ts";
import type { Shape } from "./validators.ts";

export interface TableDefinition<
  S extends Shape = Shape,
  I extends Record<string, readonly (keyof S & string)[]> = Record<string, readonly (keyof S & string)[]>,
> {
  readonly fields: S;
  readonly indexes: I;
  index<const N extends string, const K extends readonly (keyof S & string)[]>(
    name: N,
    keys: K,
  ): TableDefinition<S, I & Record<N, K>>;
}

function table<S extends Shape, I extends Record<string, readonly (keyof S & string)[]>>(
  shape: S,
  indexes: I,
): TableDefinition<S, I> {
  return freeze({
    fields: shape,
    indexes,
    index<const N extends string, const K extends readonly (keyof S & string)[]>(name: N, keys: K) {
      identifier(name);
      if (Object.keys(indexes).some((key) => key.toLowerCase() === name.toLowerCase()))
        throw new Error(`Duplicate index: ${name}`);
      if (
        Object.keys(indexes).length >= 16 ||
        keys.length === 0 ||
        keys.length > 8 ||
        new Set(keys).size !== keys.length
      )
        throw new Error("Invalid index size");
      for (const key of keys) {
        const field = shape[key];
        if (
          !field ||
          !["boolean", "number", "integer", "string", "id", "player", "session", "enum"].includes(field.schema.type)
        )
          throw new Error(`Index requires a scalar field: ${key}`);
      }
      return table(shape, { ...indexes, [name]: [...keys] as K } as I & Record<N, K>);
    },
  });
}

export function defineTable<const S extends Shape>(shape: S): TableDefinition<S, {}> {
  fields(shape);
  return table({ ...shape }, {});
}

export interface SchemaDefinition<T extends Record<string, TableDefinition> = Record<string, TableDefinition>> {
  readonly tables: T;
  readonly contract: Record<string, { fields: ReturnType<typeof fields>; indexes: Record<string, readonly string[]> }>;
}

export function defineSchema<const T extends Record<string, TableDefinition>>(tables: T): SchemaDefinition<T> {
  if (Object.keys(tables).length > 128) throw new Error("Too many tables");
  const names = new Set<string>();
  const definitions = new Set<TableDefinition>();
  const contract = Object.fromEntries(
    Object.entries(tables).map(([name, definition]) => {
      identifier(name);
      if (names.has(name.toLowerCase()) || definitions.has(definition))
        throw new Error(`Duplicate table registration: ${name}`);
      names.add(name.toLowerCase());
      definitions.add(definition);
      return [name, { fields: fields(definition.fields), indexes: definition.indexes }];
    }),
  );
  return freeze({ tables: { ...tables }, contract });
}

export { v, apiValidator } from "./validators.ts";
export type {
  Validator,
  ObjectValidator,
  OptionalValidator,
  Shape,
  Infer,
  InferObject,
  Id,
  PlayerId,
  SessionId,
  JsonValue,
} from "./validators.ts";
