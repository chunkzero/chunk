import { defineApp, v } from "#chunk";

export default defineApp({
  id: "arena",
  runtime: { machineProfile: "local", maxPlayers: 16 },
  implementations: {
    default: { config: v.object({ label: v.string() }) },
  },
  destinations: {
    standard: {
      implementation: "default",
      key: "arena",
      config: { label: "Arena" },
    },
    large: {
      implementation: "default",
      key: "arena-large",
      machineProfile: "large",
      maxPlayers: 32,
      config: { label: "Large arena" },
    },
  },
});
