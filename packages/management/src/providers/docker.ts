import { type Machine, type MachineSpec, type MachineState, OwnershipError, type Provider } from "./provider.ts";

export interface DockerProviderOptions {
  /** Path of the Docker-compatible API's unix socket. */
  socketPath: string;
  /** A user-defined network every machine joins; created when missing and never removed. */
  network: string;
  /** Identifies this chunk install; the provider only touches containers and volumes labelled with it. */
  installId: string;
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
const installLabel = "chunk.install";
const machineLabel = "chunk.machine";
const stopTimeoutSeconds = 10;

type Labels = Record<string, string> | null | undefined;

interface Inspection {
  Id: string;
  Name: string;
  Config: { Labels?: Labels };
  State: { Status: string };
  NetworkSettings?: { Networks?: Record<string, { IPAddress?: string }> };
}

interface Volume {
  Name: string;
  Labels?: Labels;
}

/** The socket path from a DOCKER_HOST value such as unix:///run/user/1000/podman/podman.sock; throws for non-unix hosts. */
export function socketPathFrom(dockerHost: string): string {
  const url = new URL(dockerHost);
  if (url.protocol !== "unix:" || !url.pathname)
    throw new Error(`unsupported DOCKER_HOST ${dockerHost}; expected unix://`);
  return url.pathname;
}

/** Runs machines as containers through the Docker Engine API, which Podman also serves. */
export function dockerProvider({ socketPath, network, installId }: DockerProviderOptions): Provider {
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

  const ownership = (name: string) => ({ [installLabel]: installId, [machineLabel]: name });
  const owned = (labels: Labels) => labels?.[installLabel] === installId;
  const ownedFor = (labels: Labels, machine: string) => owned(labels) && labels?.[machineLabel] === machine;

  const inspect = async (id: string) => {
    const response = await call("GET", `/containers/${encodeURIComponent(id)}/json`, { allow: [404] });
    return response.status === 404 ? undefined : ((await response.json()) as Inspection);
  };

  const nameOf = (inspection: Inspection) => inspection.Name.replace(/^\//, "");

  const machineFrom = (inspection: Inspection): Machine => {
    const name = nameOf(inspection);
    const ip = inspection.NetworkSettings?.Networks?.[network]?.IPAddress;
    return { id: inspection.Id, name, state: stateOf(inspection.State.Status), addresses: ip ? [name, ip] : [name] };
  };

  const refuseForeign = (inspection: Inspection | undefined, what: string) => {
    if (inspection && !owned(inspection.Config.Labels)) throw new OwnershipError(`container ${what}`);
    return inspection;
  };

  const status = async (id: string): Promise<Machine> => {
    const inspection = refuseForeign(await inspect(id), id);
    return inspection ? machineFrom(inspection) : { id, name: "", state: "missing", addresses: [] };
  };

  // Inspecting also matches ID prefixes, so a name only counts when it is the container's exact name.
  const named = async (name: string) => {
    const inspection = await inspect(name);
    return inspection && nameOf(inspection) === name ? inspection : undefined;
  };

  const existing = async (id: string) => {
    const inspection = refuseForeign(await inspect(id), id);
    if (!inspection) throw new ProviderError(404, `no such machine ${id}`);
    return machineFrom(inspection);
  };

  // An existing container is adopted only when it carries this install's labels and every label the spec asks for.
  const adopt = (spec: MachineSpec, inspection: Inspection) => {
    const labels = inspection.Config.Labels;
    const wanted = Object.entries({ ...spec.labels, ...ownership(spec.name) });
    if (!wanted.every(([key, value]) => labels?.[key] === value)) throw new OwnershipError(`container ${spec.name}`);
    return machineFrom(inspection);
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

  // Docker returns an existing volume from create while Podman answers 409, so both paths check the labels.
  const createVolume = async (spec: MachineSpec, name: string) => {
    let response = await call("POST", "/volumes/create", {
      body: { Name: name, Labels: { ...spec.labels, ...ownership(spec.name) } },
      allow: [409],
    });
    if (response.status === 409) response = await call("GET", `/volumes/${encodeURIComponent(name)}`);
    const volume = (await response.json()) as Volume;
    if (!ownedFor(volume.Labels, spec.name)) throw new OwnershipError(`volume ${name}`);
  };

  const createContainer = async (spec: MachineSpec) => {
    const body = {
      Image: spec.image,
      ...(spec.command ? { Cmd: spec.command } : {}),
      Env: Object.entries(spec.env).map(([key, value]) => `${key}=${value}`),
      Labels: { ...spec.labels, ...ownership(spec.name) },
      HostConfig: {
        Memory: spec.memoryMib * 1024 * 1024,
        NanoCpus: Math.round(spec.cpus * 1e9),
        RestartPolicy: { Name: spec.restart ? "unless-stopped" : "no" },
        Binds: spec.volumes.map(({ name, path }) => `${name}:${path}`),
      },
      NetworkingConfig: { EndpointsConfig: { [network]: {} } },
    };
    return call("POST", `/containers/create?${new URLSearchParams({ name: spec.name })}`, { body, allow: [404] });
  };

  return {
    async create(spec) {
      const found = await named(spec.name);
      if (found) return adopt(spec, found);
      await call("POST", "/networks/create", { body: { Name: network }, allow: [409] });
      for (const volume of spec.volumes) await createVolume(spec, volume.name);
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
        if (raced) return adopt(spec, raced);
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

    status,

    async find(name) {
      const inspection = refuseForeign(await named(name), name);
      return inspection && machineFrom(inspection);
    },

    async destroy(name) {
      const inspection = refuseForeign(await named(name), name);
      // `v` also removes the anonymous volumes the image declares, such as the JVM runner's cache.
      if (inspection) {
        await call("DELETE", `/containers/${encodeURIComponent(inspection.Id)}?force=true&v=true`, { allow: [404] });
      }
      const filters = JSON.stringify({ label: [`${installLabel}=${installId}`, `${machineLabel}=${name}`] });
      const response = await call("GET", `/volumes?${new URLSearchParams({ filters })}`);
      const { Volumes } = (await response.json()) as { Volumes?: Volume[] | null };
      for (const volume of Volumes ?? []) {
        if (ownedFor(volume.Labels, name)) {
          await call("DELETE", `/volumes/${encodeURIComponent(volume.Name)}`, { allow: [404] });
        }
      }
    },
  };
}

function stateOf(status: string): MachineState {
  if (status === "running" || status === "restarting") return "running";
  if (status === "paused") return "suspended";
  return "stopped";
}
