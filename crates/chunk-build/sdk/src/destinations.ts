import { freeze } from "./validators.ts";
import type { Destination } from "./validators.ts";

const definition = Symbol.for("@chunk/destination");
export interface DestinationOptions extends Destination {
  readonly overflow?: "replicate" | "reject";
  readonly emptyTimeoutSeconds?: number;
}
export interface DestinationDefinition {
  readonly [definition]: true;
  readonly destination: Readonly<Destination>;
  readonly contract: {
    readonly destination: Readonly<Destination>;
    readonly overflow: "replicate" | "reject";
    readonly empty_timeout_seconds: number;
  };
}

/** Declares a deployment-pinned capacity pool; it never provisions a process itself. */
export function defineDestination(options: DestinationOptions): DestinationDefinition {
  if (
    !options ||
    Object.keys(options).some(
      (key) => !["key", "session_type", "machine_profile", "overflow", "emptyTimeoutSeconds"].includes(key),
    )
  ) {
    throw new Error("Unsupported destination option; groups, rosters and parameters are not supported");
  }
  const { key, session_type, machine_profile, overflow = "replicate", emptyTimeoutSeconds = 60 } = options;
  const identifier = "[A-Za-z_][A-Za-z0-9_]{0,127}";
  if (
    typeof key !== "string" ||
    key.length === 0 ||
    new TextEncoder().encode(key).length > 128 ||
    /\p{Cc}/u.test(key) ||
    typeof session_type !== "string" ||
    !new RegExp(`^${identifier}/${identifier}$`).test(session_type) ||
    typeof machine_profile !== "string" ||
    !/^[A-Za-z0-9_-]{1,128}$/.test(machine_profile) ||
    !["replicate", "reject"].includes(overflow) ||
    !Number.isInteger(emptyTimeoutSeconds) ||
    emptyTimeoutSeconds < 1 ||
    emptyTimeoutSeconds > 86400
  ) {
    throw new Error("Invalid destination identity, overflow or empty timeout");
  }
  const destination = { key, session_type, machine_profile };
  return freeze({
    [definition]: true as const,
    destination,
    contract: { destination, overflow, empty_timeout_seconds: emptyTimeoutSeconds },
  });
}

export function isDestination(value: unknown): value is DestinationDefinition {
  return value !== null && typeof value === "object" && definition in value && value[definition] === true;
}
