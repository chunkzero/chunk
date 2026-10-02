import type { Changes } from "./changes.ts";
import type { Edge } from "./config.ts";
import type { Keys } from "./crypto.ts";
import type { Database } from "./db.ts";
import type { LogStoreIssuer } from "./logstore/issuer.ts";
import type { ArchiveLimits } from "./releases/archive.ts";
import type { ReleaseStore } from "./releases/store.ts";

/** Everything the services share. Installs swap implementations here, for example a different release store. */
export interface Deps {
  /** Drizzle over the service's pool. An install's own tables can be queried and joined with chunk's schema tables. */
  db: Database;
  keys: Keys;
  releases: ReleaseStore;
  /** Bounds how much a release archive may expand while it is verified. */
  archiveLimits: ArchiveLimits;
  resolveTxt: (hostname: string) => Promise<string[][]>;
  /** How clients reach this service. */
  publicUrl: string;
  /** Where players reach environments; unset leaves environments without hostnames. */
  edge: Edge | undefined;
  /** Where environments replicate their logs; unset turns replication off. */
  logStore: LogStoreIssuer | undefined;
  /** The image template JVM machines run (see `Machines.jvmImage`); unset, releases can't be deployed. */
  jvmImage: string | undefined;
  changes: Changes;
  /** Aborts when the service shuts down; open streams end then. */
  shutdown: AbortSignal;
}
