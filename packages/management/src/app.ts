import { resolveTxt } from "node:dns/promises";
import { join } from "node:path";

import { ensureEdgeToken, ensureOperatorToken } from "./auth/tokens.ts";
import { listenForChanges } from "./changes.ts";
import type { Config } from "./config.ts";
import { deriveKeys } from "./crypto.ts";
import { connect, migrate } from "./db.ts";
import type { Deps } from "./deps.ts";
import { startReconciler } from "./environments/reconciler.ts";
import { logStoreIssuer } from "./logstore/issuer.ts";
import { dockerProvider, socketPathFrom } from "./providers/docker.ts";
import { localReleaseStore } from "./releases/local-store.ts";
import { maxArchiveBytes } from "./releases/store.ts";
import { createHandler } from "./server.ts";

/** Migrates the database and serves the single-tenant install described by `config`. */
export async function start(config: Config) {
  const sql = connect(config.databaseUrl);
  await migrate(sql);
  if (config.operatorToken) await ensureOperatorToken(sql, config.operatorToken);
  if (config.edgeToken) await ensureEdgeToken(sql, config.edgeToken);
  const keys = deriveKeys(config.secretKey);
  const shutdown = new AbortController();
  const deps: Deps = {
    sql,
    keys,
    releases: await localReleaseStore({
      directory: join(config.dataDir, "releases"),
      keys,
      publicUrl: config.publicUrl,
    }),
    archiveLimits: config.archiveLimits,
    resolveTxt,
    publicUrl: config.publicUrl,
    edge: config.edge,
    logStore: config.logStore && logStoreIssuer(config.logStore),
    jvmImage: config.machines?.jvmImage,
    changes: await listenForChanges(sql),
    shutdown: shutdown.signal,
  };
  const { machines } = config;
  const [installation] = await sql<{ id: string }[]>`select id from installation`;
  const reconciler =
    machines &&
    startReconciler(deps, {
      ...machines,
      provider: dockerProvider({
        socketPath: socketPathFrom(machines.dockerHost),
        network: machines.network,
        installId: installation?.id ?? "",
      }),
    });
  const server = Bun.serve({
    hostname: config.host,
    port: config.port,
    maxRequestBodySize: Number(maxArchiveBytes) + 1024 * 1024,
    fetch: createHandler(deps, { dashboardDir: config.dashboardDir }),
  });
  return {
    url: server.url,
    async stop() {
      // Streams never end on their own, and stopping the server waits for every open request.
      shutdown.abort();
      await server.stop();
      await reconciler?.stop();
      await sql.end({ timeout: 5 });
    },
  };
}
