import { defineApp } from "#chunk";

export default defineApp({
  id: "lobby",
  runtime: { maxPlayers: 50 },
  worlds: { lobby: { source: "worlds/lobby.polar" } },
  destinations: {
    main: { implementation: "default", key: "lobby" },
  },
});
