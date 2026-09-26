import type { Machine, MachineSpec, Provider } from "../src/providers/provider.ts";

type Method = "create" | "start" | "suspend" | "status";

/**
 * An in-memory provider. Machine IDs are their names; like Docker, a machine only gets an IP when it starts, and a
 * new one each time. `hooks` runs before a method, and throwing from it fails the call; the `create` hook runs once
 * the machine exists, so throwing from it models a lost reply.
 */
export function fakeProvider() {
  const machines = new Map<string, { spec: MachineSpec; machine: Machine }>();
  const hooks: Partial<Record<Method, ((id: string) => Promise<void> | void) | undefined>> = {};
  let nextAddress = 2;
  const get = (id: string) => {
    const entry = machines.get(id);
    if (!entry) throw new Error(`no such machine ${id}`);
    return entry;
  };
  const set = (id: string, state: Machine["state"], addresses?: string[]) => {
    const entry = get(id);
    entry.machine = { ...entry.machine, state, ...(addresses ? { addresses } : {}) };
    return entry.machine;
  };
  const provider: Provider = {
    async create(spec) {
      const existing = machines.get(spec.name);
      if (existing) return existing.machine;
      const machine: Machine = { id: spec.name, name: spec.name, state: "stopped", addresses: [spec.name] };
      machines.set(spec.name, { spec, machine });
      await hooks.create?.(spec.name);
      return machine;
    },
    async start(id) {
      await hooks.start?.(id);
      const { machine } = get(id);
      if (machine.state === "suspended") return set(id, "running");
      if (machine.state === "running") return machine;
      return set(id, "running", [machine.name, `10.0.0.${nextAddress++}`]);
    },
    async suspend(id) {
      await hooks.suspend?.(id);
      return get(id).machine.state === "running" ? set(id, "suspended") : get(id).machine;
    },
    stop: async (id) => set(id, "stopped", [get(id).machine.name]),
    async status(id) {
      await hooks.status?.(id);
      return machines.get(id)?.machine ?? { id, name: "", state: "missing", addresses: [] };
    },
    find: async (name) => machines.get(name)?.machine,
    async destroy(name) {
      machines.delete(name);
    },
  };
  return { provider, machines, hooks };
}
