import type { SessionMethodReference } from "./sessions.ts";
import { v } from "./validators.ts";
import type { Destination, PlayerIdentity } from "./validators.ts";

/** Dispatch acceptance; it does not prove display, arrival or method completion. */
export interface CommandEffectReceipt {
  readonly state: "accepted";
  readonly operationId: string;
}

export interface CommandPlayer extends Readonly<PlayerIdentity> {
  message(text: string): Promise<CommandEffectReceipt>;
  actionBar(text: string): Promise<CommandEffectReceipt>;
  title(title: string, subtitle?: string): Promise<CommandEffectReceipt>;
}

export interface CommandSession {
  /** Calls the session captured at dispatch; moves do not retarget this handle. */
  call<A, R>(method: SessionMethodReference<A, R>, args: A): Promise<R>;
  /** Accepts a typed method notification without awaiting its application result. */
  send<A, R>(method: SessionMethodReference<A, R>, args: A): Promise<CommandEffectReceipt>;
}

export interface CommandRouting {
  /** Requests a move through current destination admission policy. */
  enter(destination: Destination): Promise<CommandEffectReceipt>;
}

function text(value: string): string {
  if (typeof value !== "string" || value.length > 4096 || value.includes("\0"))
    throw new Error("Invalid command effect text");
  return value;
}

const receipt = v.object({ state: v.literal("accepted"), operationId: v.string() });

/** Targets come only from the trusted host scope, never from handler arguments. */
export function commandEffects(platform: (request: unknown) => Promise<unknown>) {
  const accept = async (request: unknown): Promise<CommandEffectReceipt> => receipt.parse(await platform(request));
  const sessionRequest = <A, R>(kind: string, method: SessionMethodReference<A, R>, args: A) => ({
    kind,
    method: { app: method.app, session: method.session, name: method.name },
    arguments: method.arguments.parse(args),
  });
  return {
    player: {
      message: (value: string) => accept({ kind: "message", text: text(value) }),
      actionBar: (value: string) => accept({ kind: "action_bar", text: text(value) }),
      title: (title: string, subtitle?: string) =>
        accept({ kind: "title", title: text(title), ...(subtitle === undefined ? {} : { subtitle: text(subtitle) }) }),
    },
    session: Object.freeze({
      call: async <A, R>(method: SessionMethodReference<A, R>, args: A): Promise<R> =>
        method.result.parse(await platform(sessionRequest("session_call", method, args))),
      send: <A, R>(method: SessionMethodReference<A, R>, args: A) =>
        accept(sessionRequest("session_send", method, args)),
    } satisfies CommandSession),
    routing: Object.freeze({
      enter: (destination: Destination) => accept({ kind: "enter", destination: v.destination().parse(destination) }),
    } satisfies CommandRouting),
  };
}
