import { resolveTxt } from "node:dns/promises";
import { join } from "node:path";

import { ensureOperatorToken } from "./auth/tokens.ts";
import type { Config } from "./config.ts";
import { deriveKeys } from "./crypto.ts";
import { connect, migrate } from "./db.ts";
import { localReleaseStore } from "./releases/local-store.ts";
import { maxArchiveBytes } from "./releases/store.ts";
import { createHandler } from "./server.ts";

/** Migrates the database and serves the single-tenant install described by `config`. */
export async function start(config: Config) {
  const sql = connect(config.databaseUrl);
  await migrate(sql);
  if (config.operatorToken) await ensureOperatorToken(sql, config.operatorToken);
  const keys = deriveKeys(config.secretKey);
  const releases = localReleaseStore({
    directory: join(config.dataDir, "releases"),
    keys,
    publicUrl: config.publicUrl,
  });
  const server = Bun.serve({
    hostname: config.host,
    port: config.port,
    maxRequestBodySize: Number(maxArchiveBytes) + 1024 * 1024,
    fetch: createHandler({
      sql,
      keys,
      releases,
      resolveTxt,
      publicUrl: config.publicUrl,
      edge: config.edge,
    }),
  });
  return {
    url: server.url,
    async stop() {
      await server.stop();
      await sql.end({ timeout: 5 });
    },
  };
}
