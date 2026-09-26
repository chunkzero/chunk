import type { Edge } from "./config.ts";
import type { Keys } from "./crypto.ts";
import type { Sql } from "./db.ts";
import type { ArchiveLimits } from "./releases/archive.ts";
import type { ReleaseStore } from "./releases/store.ts";

/** Everything the services share. Installs swap implementations here, for example a different release store. */
export interface Deps {
  sql: Sql;
  keys: Keys;
  releases: ReleaseStore;
  /** Bounds how much a release archive may expand while it is verified. */
  archiveLimits: ArchiveLimits;
  resolveTxt: (hostname: string) => Promise<string[][]>;
  /** How clients reach this service. */
  publicUrl: string;
  /** Where players reach environments; unset leaves environments without hostnames. */
  edge: Edge | undefined;
}
