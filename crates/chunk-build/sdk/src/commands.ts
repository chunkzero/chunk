import type { FunctionReference } from "./functions.ts";
import { freeze } from "./validators.ts";
import type { JsonValue, PlayerIdentity } from "./validators.ts";

const definition = Symbol.for("@chunk/command");
const routeDefinition = Symbol.for("@chunk/command-route");
declare const argumentValue: unique symbol;

export interface CommandContext {
  readonly caller: JsonValue;
  readonly player: PlayerIdentity;
  runQuery<A, R>(ref: FunctionReference<"query", A, R>, args: A): Promise<R>;
  runMutation<A, R>(ref: FunctionReference<"mutation", A, R>, args: A): Promise<R>;
}

export type SuggestionQuery = FunctionReference<"query", { input: string; cursor: number }, string[]>;
export type CommandPermission = FunctionReference<"query", Record<string, never>, boolean>;
export type CommandSuggestions = readonly string[] | SuggestionQuery;

export interface CommandArgument<T = unknown> {
  readonly parser: "boolean" | "integer" | "word" | "string" | "greedy";
  readonly min?: number;
  readonly max?: number;
  readonly suggestions?: readonly string[] | { readonly query: string };
  readonly [argumentValue]?: T;
}
export type CommandShape = Record<string, CommandArgument>;
export type CommandArguments<S extends CommandShape> = {
  [K in keyof S]: S[K] extends CommandArgument<infer T> ? T : never;
};

function label(value: string): string {
  if (!/^[a-z][a-z0-9_-]{0,63}$/.test(value)) throw new Error(`Invalid command literal: ${value}`);
  return value;
}

function suggestions(value: CommandSuggestions | undefined): CommandArgument["suggestions"] {
  if (value === undefined) return undefined;
  if (Array.isArray(value)) {
    if (value.length > 64 || new Set(value).size !== value.length) throw new Error("Invalid command suggestions");
    for (const text of value)
      if (typeof text !== "string" || text.length === 0 || text.length > 256 || /[\u0000-\u001f]/.test(text))
        throw new Error("Invalid command suggestion");
    return [...value];
  }
  const ref = value as SuggestionQuery;
  if (ref.kind !== "query" || typeof ref.path !== "string") throw new Error("Suggestions require a query reference");
  return { query: ref.path };
}

function argument<T>(
  parser: CommandArgument["parser"],
  options: { min?: number; max?: number; suggestions?: CommandSuggestions },
): CommandArgument<T> {
  const values = suggestions(options.suggestions);
  return freeze({
    parser,
    ...(options.min === undefined ? {} : { min: options.min }),
    ...(options.max === undefined ? {} : { max: options.max }),
    ...(values === undefined ? {} : { suggestions: values }),
  });
}

/** Minecraft command parsers are separate from database value validators. */
export const commandArg = freeze({
  boolean: () => argument<boolean>("boolean", {}),
  integer: (options: { min?: number; max?: number } = {}) => {
    for (const bound of [options.min, options.max])
      if (bound !== undefined && (!Number.isInteger(bound) || bound < -2_147_483_648 || bound > 2_147_483_647))
        throw new Error("Command integer bounds must fit a signed 32-bit integer");
    if (options.min !== undefined && options.max !== undefined && options.min > options.max)
      throw new Error("Command integer minimum exceeds maximum");
    return argument<number>("integer", options);
  },
  word: (options: { suggestions?: CommandSuggestions } = {}) => argument<string>("word", options),
  string: (options: { suggestions?: CommandSuggestions } = {}) => argument<string>("string", options),
  greedy: (options: { suggestions?: CommandSuggestions } = {}) => argument<string>("greedy", options),
});

interface RouteOptions<S extends CommandShape> {
  args?: S;
  handler: (ctx: CommandContext, args: CommandArguments<S>) => void | Promise<void>;
}

export interface CommandRoute<A = never> {
  readonly [routeDefinition]: true;
  readonly contract: {
    readonly literals: readonly string[];
    readonly arguments: readonly (CommandArgument & { readonly name: string })[];
  };
  readonly handler: (ctx: CommandContext, args: A) => void | Promise<void>;
}

/** A command route has fixed literal subcommands followed by required named arguments. */
export function commandRoute<const S extends CommandShape = {}>(
  literals: readonly string[],
  options: RouteOptions<S>,
): CommandRoute<CommandArguments<S>> {
  if (literals.length > 8) throw new Error("Command literal depth exceeds eight");
  const args = Object.entries(options.args ?? {});
  if (args.length > 16) throw new Error("Too many command arguments");
  const names = new Set<string>();
  const fields = args.map(([name, value], index) => {
    if (!/^[A-Za-z_][A-Za-z0-9_]{0,63}$/.test(name) || names.has(name.toLowerCase()))
      throw new Error(`Invalid or duplicate command argument: ${name}`);
    names.add(name.toLowerCase());
    if (!["boolean", "integer", "word", "string", "greedy"].includes(value.parser))
      throw new Error(`Unsupported command argument: ${name}`);
    if (value.parser === "greedy" && index !== args.length - 1) throw new Error("Greedy arguments must be last");
    return { ...value, name };
  });
  return freeze({
    [routeDefinition]: true as const,
    contract: { literals: literals.map(label), arguments: fields },
    handler: options.handler,
  });
}

interface CommandOptions {
  aliases?: readonly string[];
  permission?: CommandPermission;
  followPlayer?: boolean;
}

export interface CommandDefinition {
  readonly [definition]: true;
  readonly contract: {
    readonly name: string;
    readonly aliases: readonly string[];
    readonly permission?: string;
    readonly follow_player: boolean;
    readonly routes: readonly CommandRoute["contract"][];
  };
  readonly routes: readonly CommandRoute[];
}

export function command<const S extends CommandShape = {}>(
  name: string,
  options: CommandOptions & RouteOptions<S>,
): CommandDefinition;
export function command(name: string, options: CommandOptions & { routes: readonly CommandRoute[] }): CommandDefinition;
export function command(
  name: string,
  options: CommandOptions & (RouteOptions<CommandShape> | { routes: readonly CommandRoute[] }),
): CommandDefinition {
  label(name);
  const aliases = (options.aliases ?? []).map(label);
  if (aliases.length > 16 || new Set([name, ...aliases]).size !== aliases.length + 1)
    throw new Error("Duplicate or excessive command aliases");
  const routes = "routes" in options ? [...options.routes] : [commandRoute([], options)];
  if (routes.length === 0 || routes.length > 64) throw new Error("Commands require one to 64 routes");
  const paths = new Set<string>();
  for (const route of routes) {
    if (route[routeDefinition] !== true) throw new Error("Expected commandRoute descriptor");
    const path = route.contract.literals.join(" ");
    if (paths.has(path)) throw new Error(`Duplicate command route: ${path}`);
    paths.add(path);
  }
  if (options.permission && (options.permission.kind !== "query" || typeof options.permission.path !== "string"))
    throw new Error("Command permission requires a query reference");
  return freeze({
    [definition]: true as const,
    contract: {
      name,
      aliases,
      ...(options.permission === undefined ? {} : { permission: options.permission.path }),
      follow_player: options.followPlayer ?? false,
      routes: routes.map((route) => route.contract),
    },
    routes,
  });
}

export function isCommand(value: unknown): value is CommandDefinition {
  return value !== null && typeof value === "object" && definition in value && value[definition] === true;
}

interface RawCommandContext {
  readonly caller: JsonValue;
  runQuery(path: string, args: unknown): Promise<unknown>;
  runMutation(path: string, args: unknown): Promise<unknown>;
}

/** Compiler adapter. The platform supplies the authorized route and authenticated player. */
export async function invokeCommand(
  descriptor: CommandDefinition,
  raw: RawCommandContext,
  payload: { route: number; arguments: Record<string, unknown>; player: PlayerIdentity },
): Promise<null> {
  if (!Number.isInteger(payload.route) || payload.route < 0 || payload.route >= descriptor.routes.length)
    throw new Error("Unknown command route");
  const context: CommandContext = Object.freeze({
    caller: freeze(raw.caller),
    player: freeze(payload.player),
    runQuery: <A, R>(ref: FunctionReference<"query", A, R>, args: A) => raw.runQuery(ref.path, args) as Promise<R>,
    runMutation: <A, R>(ref: FunctionReference<"mutation", A, R>, args: A) =>
      raw.runMutation(ref.path, args) as Promise<R>,
  });
  const route = descriptor.routes[payload.route] as CommandRoute<Record<string, unknown>>;
  await route.handler(context, freeze(payload.arguments));
  return null;
}
