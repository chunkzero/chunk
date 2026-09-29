import type { Machine, MachineSpec, Provider } from "../src/providers/provider.ts";

type Method = "creating" | "create" | "start" | "suspend" | "status" | "destroy";

/**
 * An in-memory provider. Every machine gets a new ID, `<name>@<n>`; like Docker, a machine only gets an IP when it
 * starts, and a new one each time, and a name holds at most one machine. `hooks` runs before a method with the
 * machine's name, and throwing from it fails the call. `creating` runs once a new machine's volumes exist but before the
 * machine does, which models a slow create; the `create` hook runs once it exists, so throwing from it models a lost
 * reply. `boots` counts, per name, starts of a stopped machine. With `behaviour.suspendStops`, suspending stops a
 * machine, as a provider that cannot keep a machine's memory does.
 */
export function fakeProvider() {
  const behaviour = { suspendStops: false };
  const machines = new Map<string, { spec: MachineSpec; machine: Machine }>();
  /** Volume names by the machine name they were created for. */
  const volumes = new Map<string, string[]>();
  const boots = new Map<string, number>();
  const hooks: Partial<Record<Method, ((name: string) => Promise<void> | void) | undefined>> = {};
  let nextId = 1;
  let nextAddress = 2;
  const nameOf = (id: string) => (id.includes("@") ? id.slice(0, id.lastIndexOf("@")) : id);
  const current = (id: string) => {
    const entry = machines.get(nameOf(id));
    return entry?.machine.id === id ? entry : undefined;
  };
  const get = (id: string) => {
    const entry = current(id);
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
      if (spec.volumes.length > 0)
        volumes.set(
          spec.name,
          spec.volumes.map(({ name }) => name),
        );
      await hooks.creating?.(spec.name);
      // Another create for the name finished while this one was under way.
      const raced = machines.get(spec.name);
      if (raced) return raced.machine;
      const machine: Machine = {
        id: `${spec.name}@${nextId++}`,
        name: spec.name,
        state: "stopped",
        addresses: [spec.name],
      };
      machines.set(spec.name, { spec, machine });
      await hooks.create?.(spec.name);
      return machine;
    },
    async start(id) {
      await hooks.start?.(nameOf(id));
      const { machine } = get(id);
      if (machine.state === "suspended") return set(id, "running");
      if (machine.state === "running") return machine;
      boots.set(machine.name, (boots.get(machine.name) ?? 0) + 1);
      return set(id, "running", [machine.name, `10.0.0.${nextAddress++}`]);
    },
    async suspend(id) {
      await hooks.suspend?.(nameOf(id));
      if (get(id).machine.state !== "running") return get(id).machine;
      return behaviour.suspendStops ? set(id, "stopped", [get(id).machine.name]) : set(id, "suspended");
    },
    stop: async (id) => set(id, "stopped", [get(id).machine.name]),
    async status(id) {
      await hooks.status?.(nameOf(id));
      return current(id)?.machine ?? { id, name: "", state: "missing", addresses: [] };
    },
    find: async (name) => machines.get(name)?.machine,
    async destroy(name, options) {
      await hooks.destroy?.(name);
      if (options && machines.get(name)?.machine.id !== options.id) return;
      machines.delete(name);
      volumes.delete(name);
    },
    list: async () => [
      ...[...machines.values()].map(({ machine }) => machine),
      ...[...volumes.keys()]
        .filter((name) => !machines.has(name))
        .map((name): Machine => ({ id: "", name, state: "missing", addresses: [] })),
    ],
  };
  return { provider, machines, volumes, boots, hooks, behaviour };
}
