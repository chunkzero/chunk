import { resolveTxt } from "node:dns/promises";
import { join } from "node:path";

import type { ConnectRouter } from "@connectrpc/connect";

import { ensureEdgeToken, ensureOperatorToken, tokenAuthenticator } from "./auth/tokens.ts";
import { listenForChanges } from "./changes.ts";
import type { Config } from "./config.ts";
import { deriveKeys } from "./crypto.ts";
import { connect, migrate } from "./db.ts";
import type { Deps } from "./deps.ts";
import { startReconciler } from "./environments/reconciler.ts";
import { logStoreIssuer } from "./logstore/issuer.ts";
import { dockerProvider, socketPathFrom } from "./providers/docker.ts";
import type { Provider } from "./providers/provider.ts";
import { localReleaseStore } from "./releases/local-store.ts";
import { maxArchiveBytes } from "./releases/store.ts";
import type { Authenticator } from "./rpc/caller.ts";
import { createHandler, type HandlerOptions } from "./server.ts";

/** What an install built on this service adds to it. */
export interface Extensions {
  /** Runs environments' machines instead of the Docker provider when `config.machines` is set. */
  provider?: (deps: Deps, installId: string) => Provider;
  /** A directory of `*.sql` migrations applied after chunk's; see `migrate`. */
  migrations?: string;
  /** Registers more services; a service registered again here replaces the default one. */
  extend?: (router: ConnectRouter, deps: Deps) => void;
  /** Replaces the authenticator; `tokens` is chunk's own, for the bearers the install does not recognize itself. */
  authenticator?: (tokens: Authenticator, deps: Deps) => Authenticator;
  /** Starts background work once the service is set up; the returned function stops it before the database closes. */
  start?: (deps: Deps) => Promise<() => Promise<void>>;
}

/** Migrates the database and serves the single-tenant install described by `config`, with `extensions` added. */
export async function start(config: Config, extensions: Extensions = {}) {
  const sql = connect(config.databaseUrl);
  await migrate(sql, extensions.migrations);
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
      machineUrl: config.machines?.managementUrl ?? config.publicUrl,
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
  const installId = installation?.id ?? "";
  const reconciler =
    machines &&
    startReconciler(
      deps,
      {
        ...machines,
        provider:
          extensions.provider?.(deps, installId) ??
          dockerProvider({ socketPath: socketPathFrom(machines.dockerHost), network: machines.network, installId }),
      },
      config.databaseUrl,
    );
  const stopExtensions = await extensions.start?.(deps);
  const { extend, authenticator } = extensions;
  const options: HandlerOptions = { dashboardDir: config.dashboardDir };
  if (extend) options.extend = (router) => extend(router, deps);
  if (authenticator) options.authenticator = authenticator(tokenAuthenticator(sql), deps);
  const server = Bun.serve({
    hostname: config.host,
    port: config.port,
    maxRequestBodySize: Number(maxArchiveBytes) + 1024 * 1024,
    fetch: createHandler(deps, options),
  });
  return {
    url: server.url,
    async stop() {
      // Streams never end on their own, and stopping the server waits for every open request.
      shutdown.abort();
      await server.stop();
      await stopExtensions?.();
      await reconciler?.stop();
      await sql.end({ timeout: 5 });
    },
  };
}
