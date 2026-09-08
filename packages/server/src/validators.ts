export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue }
declare const idBrand: unique symbol
export type Id<Table extends string> = string & { readonly [idBrand]: { table: Table } }
export type PlayerId = string & { readonly [idBrand]: "player" }
export type SessionId = string & { readonly [idBrand]: "session" }

export type Schema =
  | { type: "null" | "boolean" | "number" | "integer" | "string" | "player" | "session" }
  | { type: "id"; table: string }
  | { type: "literal"; value: null | boolean | number | string }
  | { type: "array"; items: Schema }
  | { type: "object"; fields: Record<string, { schema: Schema; optional: boolean }> }
  | { type: "union"; variants: Schema[] }

export interface Validator<T> {
  readonly schema: Schema
  readonly optional: false
  parse(value: unknown): T
}
export interface OptionalValidator<T> {
  readonly schema: Schema
  readonly optional: true
  parse(value: unknown): T
}
export type Shape = Record<string, Validator<unknown> | OptionalValidator<unknown>>
export type Infer<V extends { parse(value: unknown): unknown }> = ReturnType<V["parse"]>
export type InferObject<S extends Shape> =
  { [K in keyof S as S[K]["optional"] extends true ? never : K]: Infer<S[K]> } &
  { [K in keyof S as S[K]["optional"] extends true ? K : never]?: Infer<S[K]> }

export function identifier(name: string): void {
  if (!/^[A-Za-z][A-Za-z0-9_]{0,63}$/.test(name) || name.toLowerCase().startsWith("sqlite_")) {
    throw new Error(`Invalid schema identifier: ${name}`)
  }
}

export function freeze<T>(value: T): T {
  if (value !== null && typeof value === "object") {
    for (const item of Object.values(value)) freeze(item)
    Object.freeze(value)
  }
  return value
}

export function validator<T>(schema: Schema): Validator<T> {
  return freeze({ schema, optional: false as const, parse(value: unknown): T {
    if (!accepts(schema, value, 0)) throw new Error(`Value does not match ${schema.type}`)
    return value as T
  } })
}

function accepts(schema: Schema, value: unknown, depth: number): boolean {
  if (depth > 32) return false
  switch (schema.type) {
    case "null": return value === null
    case "boolean": return typeof value === "boolean"
    case "number": return typeof value === "number" && Number.isFinite(value) && (!Number.isInteger(value) || Number.isSafeInteger(value))
    case "integer": return typeof value === "number" && Number.isSafeInteger(value)
    case "string": return typeof value === "string" && !/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(value)
    case "player": case "session": return typeof value === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(value)
    case "id": return typeof value === "string" && value.startsWith(`${schema.table}:`) && /^[A-Za-z0-9_-]{1,128}$/.test(value.slice(schema.table.length + 1))
    case "literal": return value === schema.value
    case "array": return Array.isArray(value) && Array.from({ length: value.length }, (_, i) => i)
      .every(i => Object.hasOwn(value, i) && accepts(schema.items, value[i], depth + 1))
    case "union": return schema.variants.some(item => accepts(item, value, depth + 1))
    case "object": {
      if (value === null || typeof value !== "object" || Array.isArray(value)) return false
      if (Object.getPrototypeOf(value) !== Object.prototype && Object.getPrototypeOf(value) !== null) return false
      const record = value as Record<string, unknown>
      return Object.keys(record).every(key => Object.hasOwn(schema.fields, key)) &&
        Object.entries(schema.fields).every(([key, field]) => Object.hasOwn(record, key)
          ? accepts(field.schema, record[key], depth + 1) : field.optional)
    }
  }
}

export function fields(shape: Shape): Record<string, { schema: Schema; optional: boolean }> {
  const entries = Object.entries(shape)
  if (entries.length > 64) throw new Error("Too many fields")
  const names = new Set<string>()
  return Object.fromEntries(entries.map(([name, value]) => {
    identifier(name)
    if (names.has(name.toLowerCase())) throw new Error(`Field name collision: ${name}`)
    names.add(name.toLowerCase())
    return [name, { schema: value.schema, optional: value.optional }]
  }))
}

export const v = Object.freeze({
  document: <const T extends string, const S extends Shape>(table: T, shape: S): Validator<InferObject<S> & { readonly _id: Id<T> }> => {
    identifier(table)
    return validator({ type: "object", fields: { ...fields(shape), _id: { schema: { type: "id", table }, optional: false } } })
  },
  null: () => validator<null>({ type: "null" }),
  boolean: () => validator<boolean>({ type: "boolean" }),
  number: () => validator<number>({ type: "number" }),
  integer: () => validator<number>({ type: "integer" }),
  string: () => validator<string>({ type: "string" }),
  id: <const T extends string>(table: T): Validator<Id<T>> => { identifier(table); return validator({ type: "id", table }) },
  player: () => validator<PlayerId>({ type: "player" }),
  session: () => validator<SessionId>({ type: "session" }),
  literal: <const T extends null | boolean | number | string>(value: T): Validator<T> => {
    if (typeof value === "number" && (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value)))) throw new Error("Unsafe numeric literal")
    return validator({ type: "literal", value })
  },
  optional: <T>(value: Validator<T>): OptionalValidator<T> => freeze({ ...value, optional: true as const }),
  array: <T>(items: Validator<T>): Validator<T[]> => validator({ type: "array", items: items.schema }),
  object: <const S extends Shape>(shape: S): Validator<InferObject<S>> => validator({ type: "object", fields: fields(shape) }),
  union: <const V extends readonly Validator<unknown>[]>(...variants: V): Validator<Infer<V[number]>> => {
    if (variants.length === 0 || variants.length > 16) throw new Error("Union requires 1..16 variants")
    return validator({ type: "union", variants: variants.map(v => v.schema) })
  },
})
