import type { FunctionReference } from "./functions.ts";
import { freeze, v } from "./validators.ts";
import type { AdmissionResult, Destination, PlayerId, PlayerIdentity, ServerStatus } from "./validators.ts";

/** The gateway running a hook. It names `player` only while it holds that player's claim. */
export interface HookCaller {
  readonly kind: "gateway";
  readonly player?: PlayerId;
}
interface ReadHookContext {
  readonly caller: HookCaller;
  readonly eventId: string;
  readonly domain: string;
  runQuery<A, R>(reference: FunctionReference<"query", A, R>, args: A): Promise<R>;
}
interface PlayerHookContext extends ReadHookContext {
  readonly player: Readonly<PlayerIdentity>;
  runMutation<A, R>(reference: FunctionReference<"mutation", A, R>, args: A): Promise<R>;
}
export interface HookContexts {
  "server.ping": ReadHookContext & { readonly host: string };
  "player.login": PlayerHookContext & { readonly destination: Readonly<Destination> | null };
  "player.route": PlayerHookContext;
  "player.beforeMove": PlayerHookContext & {
    readonly sourceDomain: string;
    readonly destination: Readonly<Destination>;
  };
  "player.connect": PlayerHookContext;
  "player.disconnect": PlayerHookContext & { readonly reason: string };
  "domain.enter": PlayerHookContext;
  "domain.leave": PlayerHookContext;
}
export interface HookResults {
  "server.ping": ServerStatus;
  "player.login": AdmissionResult;
  "player.route": Destination;
  "player.beforeMove": AdmissionResult;
  "player.connect": void;
  "player.disconnect": void;
  "domain.enter": void;
  "domain.leave": void;
}
export type HookEvent = keyof HookContexts;
export type HookOptions<E extends HookEvent> = E extends "player.login" | "player.beforeMove"
  ? { readonly order?: number; readonly followPlayer?: never }
  : E extends "player.connect" | "domain.enter" | "domain.leave"
    ? { readonly order?: never; readonly followPlayer?: boolean }
    : { readonly order?: never; readonly followPlayer?: never };

const definition = Symbol.for("@chunk/hook");
export interface HookDefinition<E extends HookEvent = HookEvent> {
  readonly [definition]: true;
  readonly contract: { readonly event: E; readonly order?: number; readonly follow_player?: boolean };
  readonly handler: (ctx: HookContexts[E]) => HookResults[E] | Promise<HookResults[E]>;
}

const events: ReadonlySet<string> = new Set([
  "server.ping",
  "player.login",
  "player.route",
  "player.beforeMove",
  "player.connect",
  "player.disconnect",
  "domain.enter",
  "domain.leave",
]);

export function createHook<E extends HookEvent>(
  event: E,
  handler: (ctx: HookContexts[NoInfer<E>]) => HookResults[NoInfer<E>] | Promise<HookResults[NoInfer<E>]>,
  options?: HookOptions<NoInfer<E>>,
): HookDefinition<E> {
  if (!events.has(event) || typeof handler !== "function") throw new Error("Invalid hook event or handler");
  const { order, followPlayer } = options ?? {};
  if (options && Object.keys(options).some((key) => key !== "order" && key !== "followPlayer")) {
    throw new Error("Unknown hook option");
  }
  if (
    order !== undefined &&
    (!Number.isInteger(order) ||
      order < -2147483648 ||
      order > 2147483647 ||
      (event !== "player.login" && event !== "player.beforeMove"))
  )
    throw new Error("Only admission hooks accept a signed 32-bit integer order");
  if (
    followPlayer !== undefined &&
    (typeof followPlayer !== "boolean" || !["player.connect", "domain.enter", "domain.leave"].includes(event))
  )
    throw new Error("Hook event cannot follow a player");
  return freeze({
    [definition]: true as const,
    contract: {
      event,
      ...(order === undefined ? {} : { order }),
      ...(followPlayer === undefined ? {} : { follow_player: followPlayer }),
    },
    handler,
  });
}

export function isHook(value: unknown): value is HookDefinition {
  return value !== null && typeof value === "object" && definition in value && value[definition] === true;
}

interface RawHookContext {
  readonly caller: HookCaller;
  runQuery(path: string, args: unknown): Promise<unknown>;
  runMutation(path: string, args: unknown): Promise<unknown>;
}

/** Compiler adapter: trusted event payloads are distinct from invocation capabilities. */
export async function invokeHook(hook: HookDefinition, raw: RawHookContext, payload: object): Promise<unknown> {
  const context = Object.freeze({
    ...freeze(payload),
    caller: freeze(raw.caller),
    runQuery: <A, R>(reference: FunctionReference<"query", A, R>, args: A) =>
      raw.runQuery(reference.path, args) as Promise<R>,
    ...(hook.contract.event === "server.ping"
      ? {}
      : {
          runMutation: <A, R>(reference: FunctionReference<"mutation", A, R>, args: A) =>
            raw.runMutation(reference.path, args) as Promise<R>,
        }),
  }) as HookContexts[HookEvent];
  const result = await hook.handler(context);
  switch (hook.contract.event) {
    case "server.ping":
      return v.serverStatus().parse(result);
    case "player.login":
    case "player.beforeMove":
      return v.admissionResult().parse(result);
    case "player.route":
      return v.destination().parse(result);
    default:
      return null;
  }
}
