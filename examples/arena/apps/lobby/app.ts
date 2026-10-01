import { defineApp } from "#chunk";

export default defineApp({
  id: "lobby",
  runtime: { maxPlayers: 50 },
  destinations: {
    main: { implementation: "default", key: "lobby" },
  },
});
