import { defineApp } from "#chunk";

export default defineApp({
  id: "lobby",
  runtime: { machineProfile: "local", maxPlayers: 16 },
  destinations: {
    main: { implementation: "default", key: "lobby" },
  },
});
