/**
 * A hosting backend that runs machines for environments: the Docker/Podman provider here, Fly in Chunk Cloud. Only
 * the management service calls it; environments never hold provider credentials. Every method is idempotent, so a
 * reconciler can repeat a step after a crash.
 *
 * A provider only touches machines and volumes this install created, which it marks with ownership labels. Anything
 * else under a name it is asked about fails with `OwnershipError` and is left alone.
 *
 * A create can finish after the request it was for was released or its environment deleted, so the reconciler lists
 * the machines and volumes this install owns and destroys those nothing tracks by name.
 */
export interface Provider {
  /**
   * Creates a machine without starting it. A machine that already exists under `spec.name` is returned as it is when
   * this install created it with the same labels.
   */
  create(spec: MachineSpec): Promise<Machine>;
  /**
   * Starts or resumes the machine; a running machine stays running. IDs are never reused, so a start aimed at a
   * destroyed machine fails and starts nothing.
   */
  start(id: string): Promise<Machine>;
  /** Suspends the machine, keeping its memory where the host can; a suspended machine stays suspended. */
  suspend(id: string): Promise<Machine>;
  /** Stops the machine, keeping its volumes; a stopped machine stays stopped. */
  stop(id: string): Promise<Machine>;
  /** The machine's current state; `missing` when it does not exist. */
  status(id: string): Promise<Machine>;
  /** The machine this install created under `name`, or undefined when there is none. */
  find(name: string): Promise<Machine | undefined>;
  /**
   * Removes the machine created under `name` and every volume created for it. Missing ones are not an error, and
   * volumes are removed even when the machine is already gone, so a retry finishes an interrupted removal.
   *
   * With `id`, removes them only when the machine now under `name` has that ID, and nothing otherwise, so a stale
   * caller cannot remove a replacement created under the same name since.
   */
  destroy(name: string, options?: { id: string }): Promise<void>;
  /**
   * Every machine this install created, with the name it was created under. A name only volumes are left under, such as
   * one whose create was cut short before its machine existed, is listed as a `missing` machine with no ID.
   */
  list(): Promise<Machine[]>;
}

export interface MachineSpec {
  /** Unique per provider: lowercase letters, digits and `-`, so it is also a valid hostname. */
  name: string;
  image: string;
  /** Overrides the image's command. */
  command?: string[];
  env: Record<string, string>;
  memoryMib: number;
  cpus: number;
  /** Ownership labels; `create` adopts an existing machine only when they match. */
  labels: Record<string, string>;
  /** Named volumes that live as long as the machine. */
  volumes: { name: string; path: string }[];
  /** Whether the host restarts the machine after it exits; stateless machines are replaced instead. */
  restart: boolean;
}

export type MachineState = "stopped" | "running" | "suspended" | "missing";

export interface Machine {
  id: string;
  name: string;
  state: MachineState;
  /**
   * Hosts other machines and edges reach the machine at, without ports, the most stable first. Some may change
   * whenever the machine starts.
   */
  addresses: string[];
}

/** Something exists under a name this install uses, but this install did not create it. */
export class OwnershipError extends Error {
  constructor(what: string) {
    super(`${what} exists but was not created by this chunk install; refusing to touch it`);
    this.name = "OwnershipError";
  }
}

/** One vCPU per 2 GiB of memory, at least one. */
export function cpusFor(memoryMib: number): number {
  return Math.max(1, Math.floor(memoryMib / 2048));
}
