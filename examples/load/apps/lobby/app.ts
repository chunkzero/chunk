import { defineApp } from "#chunk";

export default defineApp({
  id: "lobby",
  runtime: { machineProfile: "lobby", maxPlayers: 128 },
  destinations: {
    main: { implementation: "default", key: "lobby" },
  },
});
