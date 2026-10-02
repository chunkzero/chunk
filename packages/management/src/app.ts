import { resolveTxt } from "node:dns/promises";
import { join } from "node:path";

import type { DescMethod } from "@bufbuild/protobuf";
import type { ConnectRouter } from "@connectrpc/connect";

import { ensureEdgeToken, ensureOperatorToken, tokenAuthenticator } from "./auth/tokens.ts";
import { listenForChanges } from "./changes.ts";
import type { Config } from "./config.ts";
import { deriveKeys } from "./crypto.ts";
import { connect, migrate } from "./db.ts";
import type { Deps } from "./deps.ts";
import { startReconciler } from "./environments/reconciler.ts";
import { type LogStoreIssuer, logStoreIssuer } from "./logstore/issuer.ts";
import { dockerProvider, socketPathFrom } from "./providers/docker.ts";
import type { Provider } from "./providers/provider.ts";
import { localReleaseStore } from "./releases/local-store.ts";
import { s3ReleaseStore } from "./releases/s3-store.ts";
import { maxArchiveBytes } from "./releases/store.ts";
import type { Authenticator } from "./rpc/caller.ts";
import { installation } from "./schema.ts";
import { createHandler, type HandlerOptions } from "./server.ts";

/** What an install built on this service adds to it. */
export interface Extensions {
  /** Runs environments' machines instead of the Docker provider when `config.machines` is set. */
  provider?: (deps: Deps, installId: string) => Provider;
  /**
   * A drizzle-kit migrations folder for the install's own tables (`out` in its drizzle-kit config), applied after
   * chunk's on start and recorded in `drizzle.extension_migrations`; see `migrate`. Its schema may reference chunk's
   * tables, which the package exports.
   */
  migrations?: string;
  /** Registers more services; a service registered again here replaces the default one. */
  extend?: (router: ConnectRouter, deps: Deps) => void;
  /** Methods of the services `extend` registers that anyone may call without a bearer token; each checks its request. */
  publicMethods?: readonly DescMethod[];
  /**
   * Replaces the authenticator; `tokens` is chunk's own, for the bearers the install does not recognize itself. Setting
   * `owners` on a person's caller limits them to those owners' projects; it is resolved again on every request.
   */
  authenticator?: (tokens: Authenticator, deps: Deps) => Authenticator;
  /**
   * Ways to sign in that the dashboard offers above its API token form, each a link to `url` with `return` and `state`
   * query parameters: the dashboard path to come back to, and a single-use value binding the flow to the browser tab
   * that started it. The flow ends by redirecting to the dashboard's
   * `/signed-in?return=<return>#token=<API token>&state=<state>`, echoing `state` unchanged; the dashboard refuses a
   * token whose `state` it didn't hand out, so no other site can sign a person in to someone else's account. It then
   * checks the token, keeps it for the tab and goes back.
   */
  signInOptions?: readonly { label: string; url: string }[];
  /**
   * Serves plain HTTP paths, such as a sign-in flow's start and callback; undefined passes the request on. It is tried
   * after the RPCs, `/healthz` and the release store's URLs, and before the dashboard.
   */
  routes?: (request: Request, deps: Deps) => Promise<Response | undefined>;
  /** Issues environments' log store credentials instead of `config.logStore`. */
  logStore?: LogStoreIssuer;
  /** Starts background work once the service is set up; the returned function stops it before the database closes. */
  start?: (deps: Deps) => Promise<() => Promise<void>>;
}

/**
 * Migrates the database and serves the single-tenant install described by `config`, with `extensions` added. When
 * startup fails, whatever it had started is stopped again before the error is rethrown.
 */
export async function start(config: Config, extensions: Extensions = {}) {
  const db = connect(config.databaseUrl);
  const shutdown = new AbortController();
  let server: ReturnType<typeof Bun.serve> | undefined;
  let stopExtensions: (() => Promise<void>) | undefined;
  let reconciler: { stop(): Promise<void> } | undefined;
  const stop = () => {
    // Streams never end on their own, and stopping the server waits for every open request.
    shutdown.abort();
    return runAll([
      () => server?.stop(),
      () => stopExtensions?.(),
      () => reconciler?.stop(),
      () => db.$client.close({ timeout: 5 }),
    ]);
  };
  try {
    await migrate(db, extensions.migrations);
    if (config.operatorToken) await ensureOperatorToken(db, config.operatorToken);
    if (config.edgeToken) await ensureEdgeToken(db, config.edgeToken);
    const keys = deriveKeys(config.secretKey);
    const machineUrl = config.machines?.managementUrl ?? config.publicUrl;
    const deps: Deps = {
      db,
      keys,
      releases: config.releaseStore
        ? s3ReleaseStore(config.releaseStore)
        : await localReleaseStore({
            directory: join(config.dataDir, "releases"),
            keys,
            publicUrl: config.publicUrl,
            machineUrl,
          }),
      archiveLimits: config.archiveLimits,
      resolveTxt,
      publicUrl: config.publicUrl,
      machineUrl,
      edge: config.edge,
      logStore: extensions.logStore ?? (config.logStore && logStoreIssuer(config.logStore)),
      jvmImage: config.machines?.jvmImage,
      changes: await listenForChanges(db),
      shutdown: shutdown.signal,
    };
    const { extend, authenticator, publicMethods, signInOptions, routes } = extensions;
    const options: HandlerOptions = { dashboardDir: config.dashboardDir };
    if (extend) options.extend = (router) => extend(router, deps);
    if (publicMethods) options.publicMethods = publicMethods;
    if (signInOptions) options.signInOptions = signInOptions;
    if (routes) options.routes = (request) => routes(request, deps);
    if (authenticator) options.authenticator = authenticator(tokenAuthenticator(db), deps);
    const handler = createHandler(deps, options);
    const { machines } = config;
    if (machines) {
      const [installed] = await db.select({ id: installation.id }).from(installation);
      const installId = installed?.id ?? "";
      const provider =
        extensions.provider?.(deps, installId) ??
        dockerProvider({ socketPath: socketPathFrom(machines.dockerHost), network: machines.network, installId });
      reconciler = startReconciler(deps, { ...machines, provider }, config.databaseUrl);
    }
    stopExtensions = await extensions.start?.(deps);
    server = Bun.serve({
      hostname: config.host,
      port: config.port,
      maxRequestBodySize: Number(maxArchiveBytes) + 1024 * 1024,
      fetch: handler,
    });
    return { url: server.url, stop };
  } catch (error) {
    await stop().catch(() => {});
    throw error;
  }
}

/** Runs every step even when earlier ones fail, then throws the first failure. */
async function runAll(steps: (() => unknown)[]): Promise<void> {
  let failure: { error: unknown } | undefined;
  for (const step of steps) {
    try {
      await step();
    } catch (error) {
      failure ??= { error };
    }
  }
  if (failure) throw failure.error;
}
