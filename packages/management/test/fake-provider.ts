import type { Machine, MachineSpec, Provider } from "../src/providers/provider.ts";

type Method = "creating" | "create" | "start" | "suspend" | "status" | "destroy";

/**
 * An in-memory provider. Machine IDs are their names; like Docker, a machine only gets an IP when it starts, and a
 * new one each time. `hooks` runs before a method, and throwing from it fails the call. `creating` runs before a new
 * machine exists, which models a slow create; the `create` hook runs once it exists, so throwing from it models a lost
 * reply.
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
      await hooks.creating?.(spec.name);
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
      await hooks.destroy?.(name);
      machines.delete(name);
    },
    list: async () => [...machines.values()].map(({ machine }) => machine),
  };
  return { provider, machines, hooks };
}
