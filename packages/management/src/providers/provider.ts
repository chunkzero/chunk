/**
 * A hosting backend that runs machines for environments: the Docker/Podman provider here, Fly in Chunk Cloud. Only
 * the management service calls it; environments never hold provider credentials. Every method is idempotent, so a
 * reconciler can repeat a step after a crash.
 */
export interface Provider {
  /** Creates a machine without starting it. A machine that already exists under `spec.name` is returned as it is. */
  create(spec: MachineSpec): Promise<Machine>;
  /** Starts or resumes the machine; a running machine stays running. */
  start(id: string): Promise<Machine>;
  /** Suspends the machine, keeping its memory where the host can; a suspended machine stays suspended. */
  suspend(id: string): Promise<Machine>;
  /** Stops the machine, keeping its volumes; a stopped machine stays stopped. */
  stop(id: string): Promise<Machine>;
  /** Removes the machine and its volumes; a missing machine is not an error. */
  destroy(id: string): Promise<void>;
  /** The machine's current state; `missing` when it does not exist. */
  status(id: string): Promise<Machine>;
}

export interface MachineSpec {
  /** Unique per provider: lowercase letters, digits, `.`, `_` and `-`. */
  name: string;
  image: string;
  /** Overrides the image's command. */
  command?: string[];
  env: Record<string, string>;
  memoryMib: number;
  cpus: number;
  labels: Record<string, string>;
  /** Named volumes that live as long as the machine. */
  volumes: { name: string; path: string }[];
}

export type MachineState = "stopped" | "running" | "suspended" | "missing";

export interface Machine {
  id: string;
  name: string;
  state: MachineState;
  /** Hosts other machines and edges reach the machine at, without ports. */
  addresses: string[];
}

/** One vCPU per 2 GiB of memory, at least one. */
export function cpusFor(memoryMib: number): number {
  return Math.max(1, Math.floor(memoryMib / 2048));
}
