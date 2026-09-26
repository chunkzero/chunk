import { afterAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";

import { dockerProvider, socketPathFrom } from "../src/providers/docker.ts";

const socketPath = process.env.DOCKER_HOST
  ? socketPathFrom(process.env.DOCKER_HOST)
  : `${process.env.XDG_RUNTIME_DIR}/podman/podman.sock`;
const socketExists = existsSync(socketPath);

test("socketPathFrom reads unix hosts and rejects others", () => {
  expect(socketPathFrom("unix:///run/user/1000/podman/podman.sock")).toBe("/run/user/1000/podman/podman.sock");
  expect(() => socketPathFrom("tcp://127.0.0.1:2375")).toThrow();
});

describe.skipIf(!socketExists)("dockerProvider", () => {
  const name = `chunk-test-${crypto.randomUUID().slice(0, 8)}`;
  const volume = `${name}-data`;
  const engine = (method: string, path: string) => fetch(`http://engine/v1.44${path}`, { method, unix: socketPath });

  afterAll(async () => {
    await engine("DELETE", `/containers/${name}?force=true`);
    await engine("DELETE", `/volumes/${volume}?force=true`);
    await engine("DELETE", `/networks/${name}`);
  });

  test("runs a machine through its lifecycle", async () => {
    const provider = dockerProvider({ socketPath, network: name });
    const spec = {
      name,
      image: "docker.io/library/busybox:1.36",
      command: ["sh", "-c", "trap 'exit 0' TERM; sleep 3600 & wait"],
      env: { GREETING: "hello" },
      memoryMib: 64,
      cpus: 1,
      labels: { "chunk.test": "true" },
      volumes: [{ name: volume, path: "/data" }],
    };

    const created = await provider.create(spec);
    expect(created).toMatchObject({ name, state: "stopped" });
    expect((await provider.create(spec)).id).toBe(created.id);

    const running = await provider.start(created.id);
    expect(running.state).toBe("running");
    expect(running.addresses.length).toBeGreaterThan(1);
    expect((await provider.start(created.id)).state).toBe("running");

    expect((await provider.suspend(created.id)).state).toBe("suspended");
    expect((await provider.suspend(created.id)).state).toBe("suspended");
    expect((await provider.start(created.id)).state).toBe("running");
    expect((await provider.stop(created.id)).state).toBe("stopped");
    expect((await provider.stop(created.id)).state).toBe("stopped");

    await provider.destroy(created.id);
    expect((await provider.status(created.id)).state).toBe("missing");
    await provider.destroy(created.id);
    expect((await engine("GET", `/volumes/${volume}`)).status).toBe(404);
  }, 120_000);
});
