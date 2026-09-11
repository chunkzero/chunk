import { documents } from "./documents.ts";
import type { Reader, Writer, Tables } from "./documents.ts";
import type { SchemaDefinition } from "./schema.ts";
import { freeze, v } from "./validators.ts";
import type { InferObject, JsonValue, Schema, Shape, Validator } from "./validators.ts";

export interface RawReader {
  scanIndex(query: {
    table: string;
    index: string;
    prefix: JsonValue[];
    start?: JsonValue;
    end?: JsonValue;
    limit: number;
  }): [string, JsonValue][];
  get(table: string, id: string): JsonValue;
  scan(table: string, start?: string | null, end?: string | null): [string, JsonValue][];
}
export interface RawWriter extends RawReader {
  put(table: string, id: string, value: JsonValue): void;
  delete(table: string, id: string): void;
}
interface RawQueryContext {
  readonly caller: JsonValue;
  readonly db: RawReader;
}
interface RawMutationContext {
  readonly caller: JsonValue;
  readonly db: RawWriter;
}
export interface QueryContext<T extends Tables> {
  readonly caller: JsonValue;
  readonly db: Reader<T>;
}
export interface MutationContext<T extends Tables> {
  readonly caller: JsonValue;
  readonly db: Writer<T>;
}
export type FunctionKind = "query" | "mutation";
export type Visibility = "public" | "internal";
const definition = Symbol.for("@chunk/function");

export interface FunctionDefinition<K extends FunctionKind = FunctionKind, A = never, R = unknown> {
  readonly [definition]: true;
  readonly contract: { kind: K; visibility: Visibility; arguments: Schema; result: Schema };
  readonly handler: (ctx: K extends "query" ? RawQueryContext : RawMutationContext, args: A) => R | Promise<R>;
}

export interface FunctionReference<K extends FunctionKind, A, R> {
  readonly path: string;
  readonly kind: K;
  readonly arguments: Validator<A>;
  readonly result: Validator<R>;
}

function builder<K extends FunctionKind>(kind: K, visibility: Visibility) {
  return <const S extends Shape, R>(options: {
    args: S;
    returns: Validator<R>;
    handler: (
      ctx: K extends "query" ? RawQueryContext : RawMutationContext,
      args: InferObject<S>,
    ) => NoInfer<R> | Promise<NoInfer<R>>;
  }): FunctionDefinition<K, InferObject<S>, R> =>
    freeze({
      [definition]: true as const,
      contract: { kind, visibility, arguments: v.object(options.args).schema, result: options.returns.schema },
      handler: options.handler,
    });
}

export const query = builder("query", "public");
export const mutation = builder("mutation", "public");
export const internalQuery = builder("query", "internal");
export const internalMutation = builder("mutation", "internal");

export function isFunction(value: unknown): value is FunctionDefinition {
  return value !== null && typeof value === "object" && definition in value && value[definition] === true;
}

export function defineFunctions<T extends Tables>(schema: SchemaDefinition<T>) {
  function typed<K extends FunctionKind>(kind: K, visibility: Visibility) {
    return <const S extends Shape, R>(options: {
      args: S;
      returns: Validator<R>;
      handler: (
        ctx: K extends "query" ? QueryContext<T> : MutationContext<T>,
        args: InferObject<S>,
      ) => NoInfer<R> | Promise<NoInfer<R>>;
    }): FunctionDefinition<K, InferObject<S>, R> =>
      builder(
        kind,
        visibility,
      )({
        args: options.args,
        returns: options.returns,
        handler: (ctx, args) =>
          options.handler(
            { caller: ctx.caller, db: documents(schema, ctx.db, kind === "mutation") } as K extends "query"
              ? QueryContext<T>
              : MutationContext<T>,
            args,
          ),
      });
  }
  return freeze({
    query: typed("query", "public"),
    mutation: typed("mutation", "public"),
    internalQuery: typed("query", "internal"),
    internalMutation: typed("mutation", "internal"),
  });
}
