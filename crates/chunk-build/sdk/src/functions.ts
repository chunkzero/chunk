import { documents } from "./documents.ts";
import type { Reader, Writer, Tables } from "./documents.ts";
import type { SchemaDefinition } from "./schema.ts";
import { argumentSchema, freeze } from "./validators.ts";
import type { InferObject, JsonValue, ObjectValidator, Schema, Shape, Validator } from "./validators.ts";

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

type RawContext<K extends FunctionKind> = K extends "query" ? RawQueryContext : RawMutationContext;
type Handler<C, A, R> = (ctx: C, args: A) => R | Promise<R>;
type Wrapper<K extends FunctionKind, C> = <A, R>(handler: Handler<C, A, R>) => Handler<RawContext<K>, A, R>;
type Addition<C, E> = E & { [P in keyof C]?: never };

export interface FunctionBuilder<K extends FunctionKind, C extends object> {
  <const S extends Shape, R>(options: {
    args: S | ObjectValidator<S>;
    returns: Validator<R>;
    handler: Handler<C, InferObject<S>, NoInfer<R>>;
  }): FunctionDefinition<K, InferObject<S>, R>;
  withContext<E extends object>(
    provider: (ctx: C) => Addition<C, E> | Promise<Addition<C, E>>,
  ): FunctionBuilder<K, Readonly<C & E>>;
}

function builder<K extends FunctionKind, C extends object>(
  kind: K,
  visibility: Visibility,
  wrap: Wrapper<K, C>,
): FunctionBuilder<K, C> {
  const build = <const S extends Shape, R>(options: {
    args: S | ObjectValidator<S>;
    returns: Validator<R>;
    handler: Handler<C, InferObject<S>, NoInfer<R>>;
  }): FunctionDefinition<K, InferObject<S>, R> =>
    freeze({
      [definition]: true as const,
      contract: { kind, visibility, arguments: argumentSchema(options.args), result: options.returns.schema },
      handler: wrap(options.handler),
    });
  return Object.freeze(
    Object.assign(build, {
      withContext<E extends object>(
        provider: (ctx: C) => Addition<C, E> | Promise<Addition<C, E>>,
      ): FunctionBuilder<K, Readonly<C & E>> {
        return builder<K, Readonly<C & E>>(kind, visibility, <A, R>(handler: Handler<Readonly<C & E>, A, R>) =>
          wrap<A, R>(async (ctx, args) => {
            const extra = await provider(ctx);
            return handler(extendContext(ctx, extra), args);
          }),
        );
      },
    }),
  );
}

function extendContext<C extends object, E extends object>(ctx: C, extra: E): Readonly<C & E> {
  if (
    extra === null ||
    typeof extra !== "object" ||
    (Object.getPrototypeOf(extra) !== Object.prototype && Object.getPrototypeOf(extra) !== null)
  ) {
    throw new Error("Context providers must return a plain object");
  }
  for (const key of Reflect.ownKeys(extra)) {
    if (Object.hasOwn(ctx, key)) throw new Error(`Context field already exists: ${String(key)}`);
  }
  return Object.freeze({ ...ctx, ...extra });
}

function protect<C extends { readonly caller: JsonValue }>(ctx: C): C {
  freeze(ctx.caller);
  return Object.freeze(ctx);
}

function raw<K extends FunctionKind>(kind: K, visibility: Visibility) {
  return builder<K, RawContext<K>>(kind, visibility, (handler) => (ctx, args) => handler(protect(ctx), args));
}

export const query = raw("query", "public");
export const mutation = raw("mutation", "public");
export const internalQuery = raw("query", "internal");
export const internalMutation = raw("mutation", "internal");

export function isFunction(value: unknown): value is FunctionDefinition {
  return value !== null && typeof value === "object" && definition in value && value[definition] === true;
}

export function defineFunctions<T extends Tables>(schema: SchemaDefinition<T>) {
  function typed<K extends FunctionKind>(kind: K, visibility: Visibility) {
    type Context = K extends "query" ? QueryContext<T> : MutationContext<T>;
    return builder<K, Context>(
      kind,
      visibility,
      (handler) => (ctx, args) =>
        handler(protect({ caller: ctx.caller, db: documents(schema, ctx.db, kind === "mutation") }) as Context, args),
    );
  }
  return freeze({
    query: typed("query", "public"),
    mutation: typed("mutation", "public"),
    internalQuery: typed("query", "internal"),
    internalMutation: typed("mutation", "internal"),
  });
}
