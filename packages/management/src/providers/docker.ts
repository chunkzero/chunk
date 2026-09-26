import type { Machine, MachineSpec, MachineState, Provider } from "./provider.ts";

export interface DockerProviderOptions {
  /** Path of the Docker-compatible API's unix socket. */
  socketPath: string;
  /** A user-defined network every machine joins; created when missing. */
  network: string;
}

/** An unexpected response from the engine. */
export class ProviderError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(`${message} (HTTP ${status})`);
    this.name = "ProviderError";
  }
}

// The oldest version current Docker accepts, and the one Podman 5 reports.
const apiPrefix = "/v1.44";
const managed = { "chunk.managed": "true" };
const stopTimeoutSeconds = 10;

interface Inspection {
  Id: string;
  Name: string;
  State: { Status: string };
  Mounts?: { Type: string; Name?: string }[];
  NetworkSettings?: { Networks?: Record<string, { IPAddress?: string }> };
}

/** The socket path from a DOCKER_HOST value such as unix:///run/user/1000/podman/podman.sock; throws for non-unix hosts. */
export function socketPathFrom(dockerHost: string): string {
  const url = new URL(dockerHost);
  if (url.protocol !== "unix:" || !url.pathname)
    throw new Error(`unsupported DOCKER_HOST ${dockerHost}; expected unix://`);
  return url.pathname;
}

/** Runs machines as containers through the Docker Engine API, which Podman also serves. */
export function dockerProvider({ socketPath, network }: DockerProviderOptions): Provider {
  const call = async (
    method: string,
    path: string,
    { body, allow = [] }: { body?: unknown; allow?: number[] } = {},
  ) => {
    const response = await fetch(`http://engine${apiPrefix}${path}`, {
      method,
      unix: socketPath,
      ...(body === undefined ? {} : { headers: { "content-type": "application/json" }, body: JSON.stringify(body) }),
    });
    if (response.ok || allow.includes(response.status)) return response;
    const text = await response.text();
    let message = text.trim();
    try {
      message = (JSON.parse(text) as { message?: string }).message ?? message;
    } catch {}
    throw new ProviderError(response.status, `${method} ${path}: ${message}`);
  };

  const inspect = async (id: string) => {
    const response = await call("GET", `/containers/${encodeURIComponent(id)}/json`, { allow: [404] });
    return response.status === 404 ? undefined : ((await response.json()) as Inspection);
  };

  const machineFrom = (inspection: Inspection): Machine => {
    const name = inspection.Name.replace(/^\//, "");
    const ip = inspection.NetworkSettings?.Networks?.[network]?.IPAddress;
    return { id: inspection.Id, name, state: stateOf(inspection.State.Status), addresses: ip ? [ip, name] : [name] };
  };

  const status = async (id: string): Promise<Machine> => {
    const inspection = await inspect(id);
    return inspection ? machineFrom(inspection) : { id, name: "", state: "missing", addresses: [] };
  };

  // Inspecting also matches ID prefixes, so a name only counts when it is the container's exact name.
  const named = async (name: string) => {
    const inspection = await inspect(name);
    return inspection?.Name.replace(/^\//, "") === name ? machineFrom(inspection) : undefined;
  };

  const existing = async (id: string) => {
    const machine = await status(id);
    if (machine.state === "missing") throw new ProviderError(404, `no such machine ${id}`);
    return machine;
  };

  const containerAction = (id: string, action: string, query = "") =>
    call("POST", `/containers/${encodeURIComponent(id)}/${action}${query}`, { allow: [304] });

  const pull = async (image: string) => {
    const response = await call("POST", `/images/create?${new URLSearchParams({ fromImage: image })}`);
    for (const line of (await response.text()).split("\n")) {
      if (!line.trim()) continue;
      const { error } = JSON.parse(line) as { error?: string };
      if (error) throw new ProviderError(response.status, `pull ${image}: ${error}`);
    }
  };

  const createContainer = async (spec: MachineSpec) => {
    const body = {
      Image: spec.image,
      ...(spec.command ? { Cmd: spec.command } : {}),
      Env: Object.entries(spec.env).map(([key, value]) => `${key}=${value}`),
      Labels: { ...spec.labels, ...managed },
      HostConfig: {
        Memory: spec.memoryMib * 1024 * 1024,
        NanoCpus: Math.round(spec.cpus * 1e9),
        RestartPolicy: { Name: "unless-stopped" },
        Binds: spec.volumes.map(({ name, path }) => `${name}:${path}`),
      },
      NetworkingConfig: { EndpointsConfig: { [network]: {} } },
    };
    return call("POST", `/containers/create?${new URLSearchParams({ name: spec.name })}`, { body, allow: [404] });
  };

  return {
    async create(spec) {
      await call("POST", "/networks/create", { body: { Name: network, Labels: managed }, allow: [409] });
      for (const volume of spec.volumes) {
        await call("POST", "/volumes/create", { body: { Name: volume.name, Labels: managed }, allow: [409] });
      }
      const found = await named(spec.name);
      if (found) return found;
      try {
        let response = await createContainer(spec);
        if (response.status === 404) {
          await pull(spec.image);
          response = await createContainer(spec);
        }
        if (response.status === 404) throw new ProviderError(404, `image ${spec.image} is missing after pulling it`);
        return existing(((await response.json()) as { Id: string }).Id);
      } catch (error) {
        // Another caller created the same name concurrently; Podman reports that as a 500 rather than a 409.
        const raced = await named(spec.name);
        if (raced) return raced;
        throw error;
      }
    },

    async start(id) {
      const machine = await existing(id);
      if (machine.state === "running") return machine;
      await containerAction(id, machine.state === "suspended" ? "unpause" : "start");
      return status(id);
    },

    async suspend(id) {
      const machine = await existing(id);
      if (machine.state !== "running") return machine;
      await containerAction(id, "pause");
      return status(id);
    },

    async stop(id) {
      const machine = await existing(id);
      if (machine.state === "stopped") return machine;
      if (machine.state === "suspended") await containerAction(id, "unpause");
      await containerAction(id, "stop", `?t=${stopTimeoutSeconds}`);
      return status(id);
    },

    async destroy(id) {
      const inspection = await inspect(id);
      if (!inspection) return;
      await call("DELETE", `/containers/${encodeURIComponent(inspection.Id)}?force=true`, { allow: [404] });
      for (const mount of inspection.Mounts ?? []) {
        if (mount.Type !== "volume" || !mount.Name) continue;
        const path = `/volumes/${encodeURIComponent(mount.Name)}`;
        const response = await call("GET", path, { allow: [404] });
        if (response.status === 404) continue;
        const { Labels } = (await response.json()) as { Labels?: Record<string, string> | null };
        if (Labels?.["chunk.managed"] === "true") await call("DELETE", path, { allow: [404] });
      }
    },

    status,
  };
}

function stateOf(status: string): MachineState {
  if (status === "running" || status === "restarting") return "running";
  if (status === "paused") return "suspended";
  return "stopped";
}
