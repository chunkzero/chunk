import { start } from "./app.ts";
import { loadConfig } from "./config.ts";

const app = await start(loadConfig());
console.log(`chunk management listening on ${app.url}`);

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.once(signal, () => {
    void app.stop().then(() => process.exit(0));
  });
}
