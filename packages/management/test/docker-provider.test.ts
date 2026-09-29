import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";

import { dockerProvider, socketPathFrom } from "../src/providers/docker.ts";
import { type MachineSpec, OwnershipError } from "../src/providers/provider.ts";

const socketPath = process.env.DOCKER_HOST
  ? socketPathFrom(process.env.DOCKER_HOST)
  : `${process.env.XDG_RUNTIME_DIR}/podman/podman.sock`;
const socketExists = existsSync(socketPath);

test("socketPathFrom reads unix hosts and rejects others", () => {
  expect(socketPathFrom("unix:///run/user/1000/podman/podman.sock")).toBe("/run/user/1000/podman/podman.sock");
  expect(() => socketPathFrom("tcp://127.0.0.1:2375")).toThrow();
});

describe.skipIf(!socketExists)("dockerProvider", () => {
  const prefix = `chunk-test-${crypto.randomUUID().slice(0, 8)}`;
  const image = "docker.io/library/busybox:1.36";
  const provider = dockerProvider({ socketPath, network: prefix, installId: prefix });
  const names: string[] = [];

  const engine = (method: string, path: string, body?: unknown) =>
    fetch(`http://engine/v1.44${path}`, {
      method,
      unix: socketPath,
      ...(body === undefined ? {} : { headers: { "content-type": "application/json" }, body: JSON.stringify(body) }),
    });
  const exists = async (path: string) => (await engine("GET", path)).status === 200;

  const specFor = (suffix: string, overrides: Partial<MachineSpec> = {}): MachineSpec => {
    const name = `${prefix}-${suffix}`;
    names.push(name);
    return {
      name,
      image,
      command: ["sh", "-c", "trap 'exit 0' TERM; sleep 3600 & wait"],
      env: { GREETING: "hello" },
      memoryMib: 64,
      cpus: 1,
      labels: { "chunk.test": "true" },
      volumes: [{ name: `${name}-data`, path: "/data" }],
      restart: true,
      ...overrides,
    };
  };

  beforeAll(async () => {
    await (await engine("POST", `/images/create?${new URLSearchParams({ fromImage: image })}`)).text();
  }, 120_000);

  afterAll(async () => {
    for (const name of names) {
      await engine("DELETE", `/containers/${name}?force=true`);
      await engine("DELETE", `/volumes/${name}-data?force=true`);
    }
    await engine("DELETE", `/networks/${prefix}`);
  });

  test("runs a machine through its lifecycle", async () => {
    const spec = specFor("life");
    const created = await provider.create(spec);
    expect(created).toMatchObject({ name: spec.name, state: "stopped" });
    expect((await provider.create(spec)).id).toBe(created.id);
    expect((await provider.find(spec.name))?.id).toBe(created.id);
    expect(await provider.list()).toContainEqual(created);

    const running = await provider.start(created.id);
    expect(running.state).toBe("running");
    expect(running.addresses[0]).toBe(spec.name);
    expect(running.addresses.length).toBeGreaterThan(1);
    expect((await provider.start(created.id)).state).toBe("running");

    expect((await provider.suspend(created.id)).state).toBe("suspended");
    expect((await provider.suspend(created.id)).state).toBe("suspended");
    expect((await provider.start(created.id)).state).toBe("running");
    expect((await provider.stop(created.id)).state).toBe("stopped");
    expect((await provider.stop(created.id)).state).toBe("stopped");

    await provider.destroy(spec.name);
    expect((await provider.status(created.id)).state).toBe("missing");
    expect(await provider.find(spec.name)).toBeUndefined();
    await provider.destroy(spec.name);
    expect(await exists(`/volumes/${spec.name}-data`)).toBe(false);
  }, 60_000);

  test("refuses containers this install did not create", async () => {
    const foreign = specFor("foreign");
    const response = await engine("POST", `/containers/create?name=${foreign.name}`, { Image: image });
    expect(response.status).toBe(201);
    const { Id } = (await response.json()) as { Id: string };

    await expect(provider.create(foreign)).rejects.toThrow(OwnershipError);
    await expect(provider.find(foreign.name)).rejects.toThrow(OwnershipError);
    await expect(provider.destroy(foreign.name)).rejects.toThrow(OwnershipError);
    await expect(provider.start(Id)).rejects.toThrow(OwnershipError);
    await expect(provider.status(Id)).rejects.toThrow(OwnershipError);
    expect(await exists(`/containers/${foreign.name}/json`)).toBe(true);

    const other = dockerProvider({ socketPath, network: prefix, installId: `${prefix}-other` });
    const spec = specFor("other", { volumes: [] });
    await other.create(spec);
    expect((await other.list()).map((machine) => machine.name)).toEqual([spec.name]);
    expect((await provider.list()).map((machine) => machine.name)).not.toContain(spec.name);
    await expect(provider.create(spec)).rejects.toThrow(OwnershipError);
    await expect(provider.find(spec.name)).rejects.toThrow(OwnershipError);
    await expect(provider.destroy(spec.name)).rejects.toThrow(OwnershipError);
    await expect(other.create({ ...spec, labels: { "chunk.test": "other" } })).rejects.toThrow(OwnershipError);
    expect(await exists(`/containers/${spec.name}/json`)).toBe(true);
  }, 60_000);

  test("destroy removes the anonymous volumes an image declares", async () => {
    // Postgres declares `VOLUME /var/lib/postgresql/data`, as the JVM runner image declares its cache.
    const withVolume = "docker.io/library/postgres:17";
    await (await engine("POST", `/images/create?${new URLSearchParams({ fromImage: withVolume })}`)).text();
    const { name } = await provider.create(specFor("anonymous", { image: withVolume, volumes: [], restart: false }));
    const inspection = (await (await engine("GET", `/containers/${name}/json`)).json()) as {
      Mounts: { Type: string; Name?: string }[];
    };
    const anonymous = inspection.Mounts.filter((mount) => mount.Type === "volume").map((mount) => mount.Name ?? "");
    expect(anonymous).toHaveLength(1);

    await provider.destroy(name);
    for (const volume of anonymous) expect(await exists(`/volumes/${volume}`)).toBe(false);
  }, 120_000);

  test("destroy removes volumes left behind by an interrupted destroy", async () => {
    const spec = specFor("orphan");
    await provider.create(spec);
    await engine("DELETE", `/containers/${spec.name}?force=true`);
    expect(await exists(`/volumes/${spec.name}-data`)).toBe(true);

    await provider.destroy(spec.name);
    expect(await exists(`/volumes/${spec.name}-data`)).toBe(false);
  }, 60_000);

  test("destroy under an ID leaves a replacement, and list shows names only volumes are left under", async () => {
    const spec = specFor("scoped");
    const volume = `/volumes/${spec.name}-data`;
    const first = await provider.create(spec);
    await provider.destroy(spec.name, { id: first.id });
    expect(await provider.find(spec.name)).toBeUndefined();
    expect(await exists(volume)).toBe(false);

    const replacement = await provider.create(spec);
    await provider.destroy(spec.name, { id: first.id });
    expect((await provider.find(spec.name))?.id).toBe(replacement.id);
    expect(await exists(volume)).toBe(true);

    await engine("DELETE", `/containers/${spec.name}?force=true`);
    expect(await provider.list()).toContainEqual({ id: "", name: spec.name, state: "missing", addresses: [] });
    await provider.destroy(spec.name, { id: replacement.id });
    expect(await exists(volume)).toBe(true);
    await provider.destroy(spec.name);
    expect(await exists(volume)).toBe(false);
    expect((await provider.list()).map(({ name }) => name)).not.toContain(spec.name);
  }, 60_000);

  test("maps restart to the container's restart policy", async () => {
    const policyOf = async (restart: boolean) => {
      const { name } = await provider.create(specFor(`restart-${restart}`, { restart, volumes: [] }));
      const inspection = (await (await engine("GET", `/containers/${name}/json`)).json()) as {
        HostConfig: { RestartPolicy: { Name: string } };
      };
      return inspection.HostConfig.RestartPolicy.Name;
    };
    expect(await policyOf(true)).toBe("unless-stopped");
    expect(await policyOf(false)).toBe("no");
  }, 60_000);
});
