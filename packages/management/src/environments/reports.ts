import { type Timestamp, timestampDate } from "@bufbuild/protobuf/wkt";
import type { ServiceImpl } from "@connectrpc/connect";

import { notify } from "../changes.ts";
import type { Deps } from "../deps.ts";
import type { EnvironmentService } from "../gen/chunk/management/v1/environment_pb.ts";
import { environmentOf } from "../rpc/caller.ts";
import { invalid } from "../rpc/validate.ts";
import { blockKey } from "./blocklist.ts";

const maxBatch = 1000;
const maxMessageBytes = 64 * 1024;
/**
 * The text a log batch may hold in total. JSON can escape each byte as six (`\u0000`), so even then it takes at most
 * 3 MiB of the 4 MiB RPC body cap, leaving room for the rest of the encoding.
 */
const maxLogTextBytes = 512 * 1024;
/** How long a client address that failed authentication cannot wake the environment. */
export const blockDurationMs = 10 * 60 * 1000;

type Reports = Pick<
  ServiceImpl<typeof EnvironmentService>,
  "reportUsage" | "reportLogs" | "reportMetrics" | "reportFailedAuth"
>;

/** The at-least-once and best-effort reports; each batch is written as one JSON document. */
export function reportServices({ sql }: Deps): Reports {
  return {
    async reportUsage(request, context) {
      const environmentId = environmentOf(context);
      const records = batch(request.records, "records").map((record) => {
        if (!record.id || record.id.length > 128) throw invalid("records[].id must be set, and at most 128 characters");
        const start = time(record.startTime, "records[].start_time");
        const end = time(record.endTime, "records[].end_time");
        if (end < start) throw invalid("records[].end_time is before start_time");
        return { id: record.id, start_time: start, end_time: end, player_seconds: record.playerSeconds.toString() };
      });
      await sql`
        insert into usage_records (environment_id, id, start_time, end_time, player_seconds)
        select ${environmentId}, id, start_time, end_time, player_seconds
        from jsonb_to_recordset(${JSON.stringify(records)}::text::jsonb)
          as r(id text, start_time timestamptz, end_time timestamptz, player_seconds bigint)
        on conflict do nothing`;
      return {};
    },

    async reportLogs(request, context) {
      const environmentId = environmentOf(context);
      let textBytes = 0;
      const entries = batch(request.entries, "entries").map((entry) => {
        if (!entry.instanceId) throw invalid("entries[].instance_id is required");
        if (Buffer.byteLength(entry.message) > maxMessageBytes) {
          throw invalid(`entries[].message must be at most ${maxMessageBytes} bytes`);
        }
        for (const text of [entry.message, entry.instanceId, entry.appId, entry.deploymentId]) {
          textBytes += Buffer.byteLength(text);
        }
        if (textBytes > maxLogTextBytes) throw invalid(`entries must hold at most ${maxLogTextBytes} bytes of text`);
        return {
          instance_id: entry.instanceId,
          sequence: entry.sequence.toString(),
          time: time(entry.time, "entries[].time"),
          source: entry.source,
          severity: entry.severity,
          message: entry.message,
          app_id: entry.appId,
          deployment_id: entry.deploymentId,
        };
      });
      // One writer per environment at a time, so entries commit in seq order and followers never skip one.
      const inserted = await sql.begin(async (tx) => {
        await tx`select pg_advisory_xact_lock(hashtext(${`logs/${environmentId}`}))`;
        return tx`
        insert into log_entries
          (environment_id, instance_id, sequence, time, source, severity, message, app_id, deployment_id)
        select ${environmentId}, instance_id, sequence, time, source, severity, message, app_id, deployment_id
        from jsonb_to_recordset(${JSON.stringify(entries)}::text::jsonb)
          as e(instance_id text, sequence bigint, time timestamptz, source smallint, severity smallint, message text,
            app_id text, deployment_id text)
        on conflict do nothing`;
      });
      if (inserted.count > 0) await notify(sql, { kind: "logs", environmentId });
      return {};
    },

    async reportMetrics(request, context) {
      const environmentId = environmentOf(context);
      // Postgres cannot update one row twice in a statement, so keep each series' latest sample of the batch.
      const latest = new Map<
        string,
        { instance_id: string; name: string; labels: object; time: string; value: number }
      >();
      for (const sample of batch(request.samples, "samples")) {
        if (!sample.name) throw invalid("samples[].name is required");
        const labels = Object.fromEntries(Object.entries(sample.labels).sort(([a], [b]) => a.localeCompare(b)));
        const row = {
          instance_id: sample.instanceId,
          name: sample.name,
          labels,
          time: time(sample.time, "samples[].time"),
          value: sample.value,
        };
        const key = JSON.stringify([row.instance_id, row.name, labels]);
        const previous = latest.get(key);
        if (!previous || previous.time <= row.time) latest.set(key, row);
      }
      await sql`
        insert into metric_samples (environment_id, instance_id, name, labels, time, value)
        select ${environmentId}, instance_id, name, labels, time, value
        from jsonb_to_recordset(${JSON.stringify([...latest.values()])}::text::jsonb)
          as s(instance_id text, name text, labels jsonb, time timestamptz, value double precision)
        on conflict (environment_id, instance_id, name, labels) do update
          set time = excluded.time, value = excluded.value
          where metric_samples.time <= excluded.time`;
      return {};
    },

    async reportFailedAuth(request, context) {
      const environmentId = environmentOf(context);
      const blocks = new Map<string, number>();
      for (const failure of batch(request.failures, "failures")) {
        const key = blockKey(failure.clientAddress);
        if (key === undefined) throw invalid(`client address ${JSON.stringify(failure.clientAddress)} is not an IP`);
        // A report never blocks past its own arrival plus the block duration.
        const at = Math.min(failure.time ? timestampDate(failure.time).getTime() : Date.now(), Date.now());
        blocks.set(key, Math.max(blocks.get(key) ?? 0, at + blockDurationMs));
      }
      const rows = [...blocks].map(([address, until]) => ({ address, expire_time: new Date(until).toISOString() }));
      await sql`delete from blocked_addresses where environment_id = ${environmentId} and expire_time < now()`;
      await sql`
        insert into blocked_addresses (environment_id, address, expire_time)
        select ${environmentId}, address, expire_time
        from jsonb_to_recordset(${JSON.stringify(rows)}::text::jsonb) as b(address text, expire_time timestamptz)
        where expire_time > now()
        on conflict (environment_id, address) do update
          set expire_time = greatest(blocked_addresses.expire_time, excluded.expire_time)`;
      return {};
    },
  };
}

function batch<T>(items: T[], field: string): T[] {
  if (items.length > maxBatch) throw invalid(`at most ${maxBatch} ${field} are allowed per call`);
  return items;
}

function time(value: Timestamp | undefined, field: string): string {
  if (!value) throw invalid(`${field} is required`);
  return timestampDate(value).toISOString();
}
