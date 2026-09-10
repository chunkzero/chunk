import { freeze, v } from "./validators.ts"
import type { Id, InferObject, JsonValue, Shape } from "./validators.ts"
import type { SchemaDefinition, TableDefinition } from "./schema.ts"
import type { RawReader, RawWriter } from "./functions.ts"

export type Tables = Record<string, TableDefinition>
export type Document<T extends Tables, N extends keyof T & string> = InferObject<T[N]["fields"]> & { readonly _id: Id<N> }
export const unset = Symbol("remove optional field")
type Patch<S extends Shape> = { [K in keyof S]?: ReturnType<S[K]["parse"]> | (S[K]["optional"] extends true ? typeof unset : never) }

export interface Selection<D> {
  first(): D | null
  unique(): D | null
  collect(limit: number): D[]
}
export interface IndexRange<S extends Shape, K extends readonly (keyof S & string)[]> {
  eq<N extends K[0]>(field: N, value: ReturnType<S[N]["parse"]> | null): IndexRange<S, K extends readonly [unknown, ...infer R extends (keyof S & string)[]] ? R : K>
  gte<N extends K[0]>(field: N, value: ReturnType<S[N]["parse"]>): IndexRange<S, K>
  lt<N extends K[0]>(field: N, value: ReturnType<S[N]["parse"]>): IndexRange<S, K>
}
export interface Reader<T extends Tables> {
  get<N extends keyof T & string>(table: N, id: Id<NoInfer<N>>): Document<T, N> | null
  get<N extends keyof T & string>(id: Id<N>): Document<T, N> | null
  query<N extends keyof T & string>(table: N): {
    withIndex<I extends keyof T[N]["indexes"] & string>(index: I, range?: (q: IndexRange<T[N]["fields"], T[N]["indexes"][I]>) => unknown): Selection<Document<T, N>>
  }
}
export interface Writer<T extends Tables> extends Reader<T> {
  insert<N extends keyof T & string>(table: N, value: InferObject<T[N]["fields"]>): Id<N>
  patch<N extends keyof T & string>(id: Id<N>, value: Patch<T[N]["fields"]>): void
  delete<N extends keyof T & string>(id: Id<N>): void
}

export function documents<T extends Tables>(schema: SchemaDefinition<T>, raw: RawReader, writable: boolean): Reader<T> | Writer<T> {
  function table(name: string) {
    if (!Object.hasOwn(schema.tables, name)) throw new Error(`Unknown table: ${name}`)
    return schema.tables[name]
  }
  function key(id: string) {
    const name = id.slice(0, id.indexOf(":"))
    table(name); v.id(name).parse(id)
    return name
  }
  function document<N extends keyof T & string>(name: N, id: string, value: JsonValue): Document<T, N> | null {
    return value === null ? null : Object.defineProperty({ ...v.object(table(name).fields).parse(value) }, "_id", {
      value: v.id(name).parse(id), enumerable: true,
    }) as Document<T, N>
  }
  const reader: Reader<T> = {
    get<N extends keyof T & string>(tableOrId: N | Id<N>, explicitId?: Id<N>) {
      const id = explicitId ?? tableOrId
      const name = key(id) as N
      if (explicitId !== undefined && name !== tableOrId) throw new Error(`Document ID does not belong to table: ${tableOrId}`)
      return document(name, id, raw.get(name, id))
    },
    query(name) {
      const declaration = table(name)
      return { withIndex(index, configure) {
        const fields = declaration.indexes[index]
        if (!fields) throw new Error(`Unknown index: ${index}`)
        const prefix: JsonValue[] = []
        let start: JsonValue = null, end: JsonValue = null
        let hasStart = false, hasEnd = false
        function value(field: string, value: unknown): JsonValue {
          if (fields[prefix.length] !== field) throw new Error("Index predicates must follow declared field order")
          const definition = declaration.fields[field]
          return (value === null && definition.optional ? null : definition.parse(value)) as JsonValue
        }
        const range = {
          eq(field: string, next: unknown) {
            if (hasStart || hasEnd) throw new Error("Equality must precede range bounds")
            prefix.push(value(field, next)); return range
          },
          gte(field: string, next: unknown) {
            if (hasStart) throw new Error("Duplicate lower bound")
            start = value(field, next); hasStart = true; return range
          },
          lt(field: string, next: unknown) {
            if (hasEnd) throw new Error("Duplicate upper bound")
            end = value(field, next); hasEnd = true; return range
          },
        }
        configure?.(range as unknown as Parameters<NonNullable<typeof configure>>[0])
        function collect(limit: number) {
          if (!Number.isInteger(limit) || limit < 1 || limit > 1024) throw new Error("Collection limit must be 1..1024")
          return raw.scanIndex({ table: name, index, prefix, ...(hasStart ? { start } : {}), ...(hasEnd ? { end } : {}), limit }).map(([id, value]) => document(name, id, value)!)
        }
        return freeze({ collect, first: () => collect(1)[0] ?? null, unique() {
          const rows = collect(2)
          if (rows.length > 1) throw new Error("Unique query matched multiple documents")
          return rows[0] ?? null
        } })
      } }
    },
  }
  if (!writable) return freeze(reader)
  const writer = raw as RawWriter
  return freeze({ ...reader,
    insert(name: string, input: unknown) {
      const value = v.object(table(name).fields).parse(input) as JsonValue
      const token = Array.from({ length: 4 }, () => Math.floor(Math.random() * 0x100000000).toString(16).padStart(8, "0")).join("")
      const id = `${name}:${token}`
      if (raw.get(name, id) !== null) throw new Error("Document ID collision")
      writer.put(name, id, value)
      return id
    },
    patch(id: string, patch: Record<string, unknown>) {
      const name = key(id), definition = table(name)
      const current = raw.get(name, id)
      if (current === null) throw new Error("Cannot patch a missing document")
      const value = { ...v.object(definition.fields).parse(current) } as Record<string, unknown>
      for (const [field, next] of Object.entries(patch)) {
        const validator = definition.fields[field]
        if (!validator) throw new Error(`Unknown field: ${field}`)
        if (next === unset && validator.optional) delete value[field]
        else value[field] = validator.parse(next)
      }
      writer.put(name, id, v.object(definition.fields).parse(value) as JsonValue)
    },
    delete(id: string) { writer.delete(key(id), id) },
  }) as Writer<T>
}
