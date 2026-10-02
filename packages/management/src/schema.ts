// The database's tables, the source of truth for `migrations/`: after changing them, run `pnpm db:generate` and review
// the migration it writes. Enum columns store chunk.management.v1 enum numbers.
import { sql } from "drizzle-orm";
import {
  bigint,
  bigserial,
  boolean,
  check,
  customType,
  doublePrecision,
  index,
  integer,
  pgTable,
  primaryKey,
  smallint,
  text,
  timestamp,
  unique,
  uniqueIndex,
} from "drizzle-orm/pg-core";

import type { DeploymentState, LogSeverity, LogSource, SleepingPingMode } from "./gen/chunk/management/v1/common_pb.ts";
import type { DeploymentTrigger, ReleaseState } from "./gen/chunk/management/v1/deployments_pb.ts";
import type { DomainState } from "./gen/chunk/management/v1/domains_pb.ts";
import type { CapacityState, Workload } from "./gen/chunk/management/v1/environment_pb.ts";
import type { EnvironmentState } from "./gen/chunk/management/v1/projects_pb.ts";
import type { ReleaseManifest } from "./releases/manifest.ts";

const bytea = customType<{ data: Uint8Array }>({ dataType: () => "bytea" });
/** jsonb that Bun.SQL encodes and parses itself; it would encode the JSON text Drizzle's `jsonb` sends as a string. */
const jsonb = <T>() => customType<{ data: T }>({ dataType: () => "jsonb" })();
const int8 = () => bigint({ mode: "bigint" });
const seq = () => bigserial({ mode: "bigint" }).notNull().unique();
const time = () => timestamp({ withTimezone: true });
const createTime = () => time().notNull().defaultNow();
const environmentId = () =>
  text()
    .notNull()
    .references(() => environments.id, { onDelete: "cascade" });

/** Request IDs are scoped to the calling principal and its token's project scope. */
export const idempotentRequests = pgTable(
  "idempotent_requests",
  {
    scope: text().notNull(),
    request_id: text().notNull(),
    method: text().notNull(),
    fingerprint: bytea().notNull(),
    response: bytea(),
    create_time: createTime(),
  },
  (t) => [primaryKey({ columns: [t.scope, t.request_id] })],
);

export const projects = pgTable(
  "projects",
  {
    seq: seq(),
    id: text().primaryKey(),
    owner_id: text().notNull(),
    name: text().notNull(),
    create_time: createTime(),
  },
  (t) => [unique().on(t.owner_id, t.name)],
);

/** Environment and edge tokens authenticate processes rather than people; their principal_id is empty. */
export const apiTokens = pgTable(
  "api_tokens",
  {
    seq: seq(),
    id: text().primaryKey(),
    principal_id: text().notNull(),
    name: text().notNull(),
    project_id: text().references(() => projects.id, { onDelete: "cascade" }),
    secret_hash: bytea().notNull().unique(),
    create_time: createTime(),
    expire_time: time(),
    revoke_time: time(),
    kind: text().notNull().default("person"),
    environment_id: text().references(() => environments.id, { onDelete: "cascade" }),
    /** A renewing token's expire_time slides forward each time it authenticates. */
    renews: boolean().notNull().default(false),
  },
  (t) => [
    index("api_tokens_principal").on(t.principal_id, t.seq),
    index("api_tokens_environment")
      .on(t.environment_id)
      .where(sql`${t.environment_id} is not null`),
  ],
);

export const logins = pgTable("logins", {
  id_hash: bytea().primaryKey(),
  user_code: text().notNull().unique(),
  client_name: text().notNull(),
  expire_time: time().notNull(),
  principal_id: text(),
  token_id: text(),
  /** The approved token's secret, sealed with the operator key, served to polls until expire_time. */
  token_secret: bytea(),
  create_time: createTime(),
});

export const environments = pgTable(
  "environments",
  {
    seq: seq(),
    id: text().primaryKey(),
    project_id: text()
      .notNull()
      .references(() => projects.id, { onDelete: "cascade" }),
    name: text().notNull(),
    state: smallint().$type<EnvironmentState>().notNull(),
    active_deployment_id: text().notNull().default(""),
    hostname: text().notNull().default(""),
    sleeping_ping: smallint().$type<SleepingPingMode>().notNull(),
    online_players: integer().notNull().default(0),
    forked_from_environment_id: text().notNull().default(""),
    forked_from_snapshot_id: text().notNull().default(""),
    create_time: createTime(),
    /** The desired-state revision Attach streams; every change to what core should serve advances it. */
    revision: int8()
      .notNull()
      .default(sql`1`),
    // The current core owner: its lease, the highest log epoch seen, its instance, and since when it owns the
    // environment, on management's clock.
    lease: int8()
      .notNull()
      .default(sql`0`),
    epoch: int8()
      .notNull()
      .default(sql`0`),
    owner_instance_id: text().notNull().default(""),
    owner_since: time(),
    // The latest accepted status report under the current lease.
    gateway_addresses: text().array().notNull().default([]),
    pings: jsonb<Record<string, string>>()
      .notNull()
      .default(sql`'{}'::jsonb`),
    report_sequence: int8()
      .notNull()
      .default(sql`0`),
    report_desired_revision: int8()
      .notNull()
      .default(sql`0`),
    ready_to_suspend: boolean().notNull().default(false),
    /** The login count of the latest accepted report from the lease owner's core instance. */
    report_logins: int8()
      .notNull()
      .default(sql`0`),
    // The stored wake alarm, keyed by (epoch, generation); a null due time means no alarm.
    alarm_epoch: int8()
      .notNull()
      .default(sql`0`),
    alarm_generation: int8()
      .notNull()
      .default(sql`0`),
    alarm_due_seconds: int8(),
    alarm_due_nanos: integer(),
    alarm_fired: boolean().notNull().default(false),
    // The provider machine running core, once created, and the token it was created with, sealed.
    machine_id: text().notNull().default(""),
    machine_addresses: text().array().notNull().default([]),
    machine_token: bytea(),
    // Accepted wakes that advanced the revision within the current one-minute window, for the per-environment
    // limit, and the login wakes among them that no login reported since has confirmed yet.
    wake_window_start: time().notNull().defaultNow(),
    wake_count: integer().notNull().default(0),
    wake_logins_pending: integer().notNull().default(0),
    // How long a deployment keeps running once another replaces it: after the max age its sessions take no
    // reconnects and its players move to the current deployment, and at the deadline its JVMs stop.
    drain_max_age_seconds: integer().notNull().default(10800),
    drain_deadline_seconds: integer().notNull().default(14400),
  },
  (t) => [
    unique().on(t.project_id, t.name),
    check("environments_drain_max_age_seconds_check", sql`${t.drain_max_age_seconds} > 0`),
    check("environments_drain_deadline_seconds_check", sql`${t.drain_deadline_seconds} > 0`),
    check("drain_deadline_after_max_age", sql`${t.drain_deadline_seconds} >= ${t.drain_max_age_seconds}`),
  ],
);

export const releases = pgTable(
  "releases",
  {
    project_id: text()
      .notNull()
      .references(() => projects.id, { onDelete: "cascade" }),
    id: text().notNull(),
    state: smallint().$type<ReleaseState>().notNull(),
    archive_sha256: text().notNull(),
    archive_size_bytes: int8().notNull(),
    manifest: jsonb<ReleaseManifest>(),
    create_time: createTime(),
  },
  (t) => [primaryKey({ columns: [t.project_id, t.id] })],
);

export const deployments = pgTable(
  "deployments",
  {
    seq: seq(),
    id: text().primaryKey(),
    environment_id: environmentId(),
    release_id: text().notNull(),
    state: smallint().$type<DeploymentState>().notNull(),
    trigger: smallint().$type<DeploymentTrigger>().notNull(),
    message: text().notNull().default(""),
    create_time: createTime(),
    update_time: time().notNull().defaultNow(),
    activate_time: time(),
    /** Stop the deployments this one replaces at once instead of draining them. */
    stop_previous: boolean().notNull().default(false),
  },
  (t) => [index("deployments_environment").on(t.environment_id, t.seq)],
);

/** A deleted secret keeps its row without a value, so a later set continues its versions. */
export const secrets = pgTable(
  "secrets",
  {
    environment_id: environmentId(),
    name: text().notNull(),
    version: int8().notNull(),
    ciphertext: bytea(),
    update_time: time().notNull().defaultNow(),
  },
  (t) => [primaryKey({ columns: [t.environment_id, t.name] })],
);

export const domains = pgTable(
  "domains",
  {
    seq: seq(),
    id: text().primaryKey(),
    environment_id: environmentId(),
    hostname: text().notNull(),
    state: smallint().$type<DomainState>().notNull(),
    challenge: text().notNull(),
    create_time: createTime(),
  },
  (t) => [
    unique().on(t.environment_id, t.hostname),
    // Several environments may claim a hostname, but only one can verify it (DOMAIN_STATE_VERIFIED).
    uniqueIndex("domains_verified_hostname")
      .on(t.hostname)
      .where(sql`${t.state} = 2`),
  ],
);

/**
 * Core instances another core's attach replaced; they never own the environment again. Each owned the environment
 * from `owned_since` until `superseded_time`, on management's clock.
 */
export const supersededInstances = pgTable(
  "superseded_instances",
  {
    environment_id: environmentId(),
    instance_id: text().notNull(),
    owned_since: time(),
    superseded_time: time().notNull().defaultNow(),
  },
  (t) => [primaryKey({ columns: [t.environment_id, t.instance_id] })],
);

/** Usage spans, each cut to the interval its core instance owned the environment. */
export const usageRecords = pgTable(
  "usage_records",
  {
    environment_id: environmentId(),
    id: text().notNull(),
    instance_id: text().notNull().default(""),
    start_time: time().notNull(),
    end_time: time().notNull(),
    player_seconds: int8().notNull(),
  },
  (t) => [primaryKey({ columns: [t.environment_id, t.id] })],
);

export const logEntries = pgTable(
  "log_entries",
  {
    seq: bigserial({ mode: "bigint" }).primaryKey(),
    environment_id: environmentId(),
    instance_id: text().notNull(),
    sequence: int8().notNull(),
    time: time().notNull(),
    source: smallint().$type<LogSource>().notNull(),
    severity: smallint().$type<LogSeverity>().notNull(),
    message: text().notNull(),
    app_id: text().notNull(),
    deployment_id: text().notNull(),
  },
  (t) => [
    unique().on(t.environment_id, t.instance_id, t.sequence),
    index("log_entries_environment").on(t.environment_id, t.seq),
  ],
);

/** The latest sample of each series. */
export const metricSamples = pgTable(
  "metric_samples",
  {
    environment_id: environmentId(),
    instance_id: text().notNull(),
    name: text().notNull(),
    labels: jsonb<Record<string, string>>().notNull(),
    time: time().notNull(),
    value: doublePrecision().notNull(),
  },
  (t) => [primaryKey({ columns: [t.environment_id, t.instance_id, t.name, t.labels] })],
);

/** Client addresses (IPv6 by /64) that recently failed authentication at the environment's gateways. */
export const blockedAddresses = pgTable(
  "blocked_addresses",
  {
    environment_id: environmentId(),
    address: text().notNull(),
    expire_time: time().notNull(),
  },
  (t) => [primaryKey({ columns: [t.environment_id, t.address] })],
);

/**
 * EnsureCapacity intents. The reconciler provisions PROVISIONING ones and removes the machines of releasing or failed
 * ones.
 */
export const capacityRequests = pgTable(
  "capacity_requests",
  {
    environment_id: environmentId(),
    request_id: text().notNull(),
    workload: smallint().$type<Workload>().notNull(),
    machine_profile: text().notNull(),
    release_id: text().notNull(),
    app_id: text().notNull(),
    memory_mib: integer().notNull(),
    /** The release's Java version for JVM requests, which picks its machine's runner image; null for gateways. */
    java_version: integer(),
    state: smallint().$type<CapacityState>().notNull(),
    message: text().notNull().default(""),
    machine_id: text().notNull().default(""),
    machine_addresses: text().array().notNull().default([]),
    torn_down: boolean().notNull().default(false),
    /** Whether the reconciler ever started the request's JVM machine; one that was is never started again. */
    started: boolean().notNull().default(false),
    /**
     * Whether the reconciler asked the request's suspended JVM machine to resume and has not seen it running since;
     * one that is still not running then fails its request rather than being resumed again.
     */
    resuming: boolean().notNull().default(false),
    /** The credential core minted for the request's machine, sealed under `capacityCredentialContext`. */
    credential: bytea().notNull(),
    /** `keys.fingerprint` of the plaintext credential, which retries are matched on. */
    credential_digest: bytea().notNull(),
    /** The core instance that asked for the request; superseding it releases its requests. */
    owner_instance_id: text().notNull(),
    create_time: createTime(),
  },
  (t) => [primaryKey({ columns: [t.environment_id, t.request_id] })],
);

/** Identifies this install to the provider, which labels every machine and volume it creates with it. One row. */
export const installation = pgTable("installation", {
  id: text().primaryKey(),
});

/**
 * The reconciler leader's epoch, in one row. A process that takes the leader lock bumps it, and every reconciler
 * transaction that acts first shares a lock on the row while checking its epoch is still current: a bump waits for an
 * older leader's open transaction and refuses its later ones.
 */
export const reconcilerLeader = pgTable("reconciler_leader", {
  epoch: int8().notNull(),
});
