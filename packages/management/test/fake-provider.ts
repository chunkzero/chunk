import type { Machine, MachineSpec, Provider } from "../src/providers/provider.ts";

/** An in-memory provider; machine IDs are their names and each gets a fake address. */
export function fakeProvider() {
  const machines = new Map<string, { spec: MachineSpec; machine: Machine }>();
  let nextAddress = 2;
  const get = (id: string) => {
    const entry = machines.get(id);
    if (!entry) throw new Error(`no such machine ${id}`);
    return entry;
  };
  const set = (id: string, state: Machine["state"]) => {
    const entry = get(id);
    entry.machine = { ...entry.machine, state };
    return entry.machine;
  };
  const provider: Provider = {
    async create(spec) {
      const existing = machines.get(spec.name);
      if (existing) return existing.machine;
      const machine: Machine = {
        id: spec.name,
        name: spec.name,
        state: "stopped",
        addresses: [`10.0.0.${nextAddress++}`],
      };
      machines.set(spec.name, { spec, machine });
      return machine;
    },
    start: async (id) => set(id, "running"),
    suspend: async (id) => set(id, "suspended"),
    stop: async (id) => set(id, "stopped"),
    async destroy(id) {
      machines.delete(id);
    },
    async status(id) {
      return machines.get(id)?.machine ?? { id, name: "", state: "missing", addresses: [] };
    },
  };
  return { provider, machines };
}
