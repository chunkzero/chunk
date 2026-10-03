import type { CommandDefinition } from "./commands.ts";
import { isCommand } from "./commands.ts";
import { defineDestination } from "./destinations.ts";
import type { HookDefinition, HookEvent } from "./hooks.ts";
import { isHook } from "./hooks.ts";
import { freeze, v } from "./validators.ts";
import type { Infer, ObjectValidator, Shape } from "./validators.ts";

type AnyHook = { [E in HookEvent]: HookDefinition<E> }[HookEvent];
/**
 * A resource pack players of the apps below hold: a directory with `pack.mcmeta`, or a `.zip`. Packs stack from the
 * outermost scope down to the app, each file's in declaration order; later packs override earlier ones.
 */
export interface PackOptions {
  /** Relative to the project's `assets/` in a scope, and to the app's `assets/` in an app. */
  readonly source: string;
  /** Disconnects players who decline, fail or take over 30 seconds to load the pack. */
  readonly required?: boolean;
  /** Plain text the client shows when it asks the player to accept the pack. */
  readonly prompt?: string;
}
/** A world in the app's `assets/`: a `.polar` file, or an Anvil save directory the build converts to Polar. */
export interface WorldOptions {
  readonly source: string;
  /** The inclusive chunk coordinates an Anvil save is cropped to. */
  readonly chunks?: { readonly from: readonly [number, number]; readonly to: readonly [number, number] };
}
export interface ScopeOptions {
  readonly hooks?: Readonly<Record<string, AnyHook>>;
  readonly commands?: Readonly<Record<string, CommandDefinition>>;
  readonly packs?: Readonly<Record<string, PackOptions>>;
}
export interface AppRuntime {
  readonly machineProfile?: string;
  readonly maxPlayers?: number;
}
export interface ImplementationOptions {
  readonly runtime?: AppRuntime;
  readonly config?: ObjectValidator<Shape>;
  /**
   * Whether a player who leaves one of its sessions while its release drains returns to that session on logging in
   * again. Defaults to true; set false for sessions such as lobbies that should follow the current release.
   */
  readonly reconnect?: boolean;
}
type Implementations = Readonly<Record<string, ImplementationOptions>>;
type Config<I extends ImplementationOptions> = I extends { readonly config: infer V extends ObjectValidator<Shape> }
  ? Infer<V>
  : Record<string, never>;
type AppDestination<I extends Implementations> = {
  [K in keyof I & string]: {
    readonly implementation: K;
    readonly key: string;
    readonly machineProfile?: string;
    readonly maxPlayers?: number;
    readonly config?: Config<I[K]>;
    readonly overflow?: "replicate" | "reject";
    readonly emptyTimeoutSeconds?: number;
  } & ({} extends Config<I[K]> ? {} : { readonly config: Config<I[K]> });
}[keyof I & string];
const scope = Symbol.for("@chunk/scope");
const app = Symbol.for("@chunk/app");
export interface ScopeDefinition extends ScopeOptions {
  readonly [scope]: true;
}
export interface AppDefinition extends ScopeOptions {
  readonly [app]: true;
  readonly id: string;
  readonly runtime?: AppRuntime;
  readonly implementations?: Implementations;
  readonly destinations?: Readonly<Record<string, AppDestination<Implementations>>>;
  readonly worlds?: Readonly<Record<string, WorldOptions>>;
}

const identifier = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;

function sources(entries: Readonly<Record<string, { readonly source: string }>> | undefined): void {
  for (const [name, value] of Object.entries(entries ?? {})) {
    if (!identifier.test(name) || typeof value?.source !== "string") {
      throw new Error("Worlds and packs require identifier names and a source path");
    }
  }
}

function behavior(options: ScopeOptions): void {
  sources(options.packs);
  for (const [entries, accepts] of [
    [options.hooks, isHook],
    [options.commands, isCommand],
  ] as const) {
    for (const [name, value] of Object.entries(entries ?? {})) {
      if (!identifier.test(name) || !accepts(value)) {
        throw new Error("Scope hooks and commands require named descriptors");
      }
    }
  }
}

/** Static policy inherited by apps below this scope.ts file. */
export function defineScope<const T extends ScopeOptions>(options: T): T & ScopeDefinition {
  behavior(options);
  return freeze({ ...options, [scope]: true as const });
}

/** Literal metadata is discoverable before imported handlers or generated JVM contracts exist. */
export function defineApp<const I extends Implementations = { readonly default: {} }>(
  options: ScopeOptions & {
    readonly id: string;
    readonly runtime?: AppRuntime;
    readonly implementations?: I;
    readonly destinations?: Readonly<Record<string, AppDestination<NoInfer<I>>>>;
    readonly worlds?: Readonly<Record<string, WorldOptions>>;
  },
): AppDefinition {
  behavior(options);
  sources(options.worlds);
  if (!identifier.test(options.id)) throw new Error("Invalid app id");
  for (const implementation of Object.values(options.implementations ?? { default: {} })) {
    if (implementation.config && implementation.config.schema.type !== "object") {
      throw new Error("Implementation config requires an object validator");
    }
  }
  return freeze({ ...options, [app]: true as const }) as AppDefinition;
}

export function isScope(value: unknown): value is ScopeDefinition {
  return value !== null && typeof value === "object" && scope in value && value[scope] === true;
}
export function isApp(value: unknown): value is AppDefinition {
  return value !== null && typeof value === "object" && app in value && value[app] === true;
}

/** Compiler adapter: configuration contracts are extracted only during backend compilation. */
export function appConfigurations(definition: AppDefinition) {
  return Object.entries(definition.implementations ?? {}).flatMap(([session, implementation]) =>
    implementation.config ? [{ app: definition.id, session, configuration: implementation.config.schema }] : [],
  );
}

/** Compiler adapter: resolve immutable creation values against their implementation's validator. */
export function appDestinations(definition: AppDefinition, defaults: AppRuntime) {
  const implementations: Implementations = definition.implementations ?? { default: {} };
  return Object.entries(definition.destinations ?? {}).map(([name, options]) => {
    const implementation = implementations[options.implementation];
    if (!implementation) throw new Error("Destination references an undeclared implementation");
    const runtime = { ...defaults, ...definition.runtime, ...implementation.runtime };
    const capacity = options.maxPlayers ?? runtime.maxPlayers;
    if (capacity === undefined || !Number.isInteger(capacity) || capacity < 1 || capacity > 128) {
      throw new Error("Destination requires maxPlayers between 1 and 128");
    }
    const machine_profile = options.machineProfile ?? runtime.machineProfile;
    if (machine_profile === undefined) throw new Error("Destination requires a machineProfile");
    const declared = defineDestination({
      key: options.key,
      session_type: `${definition.id}/${options.implementation}`,
      machine_profile,
      ...(options.overflow === undefined ? {} : { overflow: options.overflow }),
      ...(options.emptyTimeoutSeconds === undefined ? {} : { emptyTimeoutSeconds: options.emptyTimeoutSeconds }),
    });
    const configuration = (implementation.config ?? v.object({})).parse(options.config ?? {});
    return [
      `apps/${definition.id}/destinations/${name}`,
      { ...declared.contract, creation: { capacity, configuration } },
    ] as const;
  });
}
