import { documents } from "./documents.ts";
import type { Reader, Writer, Tables } from "./documents.ts";
import { scheduler } from "./jobs.ts";
import type { RawScheduler, Scheduler } from "./jobs.ts";
import { actionRouting } from "./routing.ts";
import type { ActionRouting } from "./routing.ts";
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
  readonly scheduler: RawScheduler;
  readonly caller: JsonValue;
  readonly db: RawWriter;
}
export interface QueryContext<T extends Tables> {
  readonly caller: JsonValue;
  readonly db: Reader<T>;
}
export interface MutationContext<T extends Tables> {
  readonly scheduler: Scheduler;
  readonly caller: JsonValue;
  readonly db: Writer<T>;
}
export interface AsyncContext {
  readonly caller: JsonValue;
  runQuery<A, R>(reference: FunctionReference<"query", A, R>, args: A): Promise<R>;
  runMutation<A, R>(reference: FunctionReference<"mutation", A, R>, args: A): Promise<R>;
}
export interface HttpRequest {
  readonly path: string;
  readonly method?: "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS";
  readonly headers?: Readonly<Record<string, string>>;
  readonly body?: string;
}
export type HttpOutcome =
  | {
      readonly state: "completed";
      readonly effectId: string;
      readonly status: number;
      readonly headers: Readonly<Record<string, string>>;
      readonly body: string;
    }
  | { readonly state: "rejected" | "unknown"; readonly effectId: string; readonly reason: string };
export interface ActionContext extends AsyncContext {
  http(binding: string, request: HttpRequest): Promise<HttpOutcome>;
  secret(name: string): Promise<string>;
  readonly invocationId: string;
  sleep(milliseconds: number): Promise<void>;
  readonly routing: ActionRouting;
}
export interface RawActionContext {
  http(binding: string, request: HttpRequest): Promise<HttpOutcome>;
  secret(name: string): Promise<string>;
  platform(request: unknown): Promise<unknown>;
  readonly caller: JsonValue;
  readonly invocationId: string;
  runQuery(path: string, args: unknown): Promise<unknown>;
  runMutation(path: string, args: unknown): Promise<unknown>;
  sleep(milliseconds: number): Promise<void>;
}
export type FunctionKind = "query" | "mutation" | "action";
export type Visibility = "public" | "internal";
const definition = Symbol.for("@chunk/function");

export interface FunctionDefinition<K extends FunctionKind = FunctionKind, A = never, R = unknown> {
  readonly [definition]: true;
  readonly contract: { kind: K; visibility: Visibility; arguments: Schema; result: Schema };
  readonly handler: (ctx: RawContext<K>, args: A) => R | Promise<R>;
}

export interface FunctionReference<K extends FunctionKind, A, R> {
  readonly path: string;
  readonly kind: K;
  readonly arguments: Validator<A>;
  readonly result: Validator<R>;
}

type RawContext<K extends FunctionKind> = K extends "query"
  ? RawQueryContext
  : K extends "mutation"
    ? RawMutationContext
    : RawActionContext;
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
  // Copying descriptors keeps `caller` lazy.
  const extended = Object.defineProperties({}, Object.getOwnPropertyDescriptors(ctx)) as C;
  return Object.freeze(Object.assign(extended, extra));
}

/** Reads the caller only when the handler does; the backend treats that read as a dependency. */
function protect<C extends object>(
  source: { readonly caller: JsonValue },
  fields: C,
): C & { readonly caller: JsonValue } {
  let caller: JsonValue;
  let read = false;
  const ctx = Object.defineProperty({}, "caller", {
    enumerable: true,
    get: () => {
      if (!read) {
        read = true;
        caller = freeze(source.caller);
      }
      return caller;
    },
  });
  return Object.freeze(Object.assign(ctx, fields)) as C & { readonly caller: JsonValue };
}

function raw<K extends "query" | "mutation">(kind: K, visibility: Visibility) {
  type Context = K extends "query"
    ? RawQueryContext
    : Omit<RawMutationContext, "scheduler"> & { readonly scheduler: Scheduler };
  return builder<K, Context>(
    kind,
    visibility,
    (handler) => (ctx, args) =>
      handler(
        protect(
          ctx,
          kind === "mutation"
            ? { db: ctx.db, scheduler: scheduler((ctx as RawMutationContext).scheduler) }
            : { db: ctx.db },
        ) as unknown as Context,
        args,
      ),
  );
}

function actionBuilder(visibility: Visibility) {
  return builder<"action", ActionContext>(
    "action",
    visibility,
    (handler) => (ctx, args) => handler(actionContext(ctx), args),
  );
}

/** Compiler adapters share the typed transaction boundary of an action. */
export function actionContext(ctx: RawActionContext): ActionContext {
  const invoke = async <K extends "query" | "mutation", A, R>(
    kind: K,
    ref: FunctionReference<K, A, R>,
    values: A,
  ): Promise<R> => {
    if (ref.kind !== kind) throw new Error("Function reference kind mismatch");
    const input = ref.arguments.parse(values);
    const result = await (kind === "query" ? ctx.runQuery(ref.path, input) : ctx.runMutation(ref.path, input));
    return ref.result.parse(result);
  };
  return protect(ctx, {
    invocationId: ctx.invocationId,
    http: (binding, request) => ctx.http(binding, request),
    secret: (name) => ctx.secret(name),
    runQuery: (ref, values) => invoke("query", ref, values),
    runMutation: (ref, values) => invoke("mutation", ref, values),
    sleep: (milliseconds) => ctx.sleep(milliseconds),
    routing: actionRouting((request) => ctx.platform(request)),
  } satisfies Omit<ActionContext, "caller">);
}

export const action = actionBuilder("public");
export const internalAction = actionBuilder("internal");
export const query = raw("query", "public");
export const mutation = raw("mutation", "public");
export const internalQuery = raw("query", "internal");
export const internalMutation = raw("mutation", "internal");

export function isFunction(value: unknown): value is FunctionDefinition {
  return value !== null && typeof value === "object" && definition in value && value[definition] === true;
}

export function defineFunctions<T extends Tables>(schema: SchemaDefinition<T>) {
  function typed<K extends "query" | "mutation">(kind: K, visibility: Visibility) {
    type Context = K extends "query" ? QueryContext<T> : MutationContext<T>;
    return builder<K, Context>(
      kind,
      visibility,
      (handler) => (ctx, args) =>
        handler(
          protect(ctx, {
            db: documents(schema, ctx.db, kind === "mutation"),
            ...(kind === "mutation" ? { scheduler: scheduler((ctx as RawMutationContext).scheduler) } : {}),
          }) as Context,
          args,
        ),
    );
  }
  return freeze({
    action,
    internalAction,
    query: typed("query", "public"),
    mutation: typed("mutation", "public"),
    internalQuery: typed("query", "internal"),
    internalMutation: typed("mutation", "internal"),
  });
}
