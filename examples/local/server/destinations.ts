import { defineDestination } from "#chunk";

export const lobby = defineDestination({
  key: "lobby",
  session_type: "lobby/default",
  machine_profile: "local",
  overflow: "replicate",
  emptyTimeoutSeconds: 60,
});
