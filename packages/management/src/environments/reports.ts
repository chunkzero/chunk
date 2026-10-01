import { type Timestamp, timestampDate } from "@bufbuild/protobuf/wkt";
import type { ServiceImpl } from "@connectrpc/connect";

import { notify } from "../changes.ts";
import type { Db } from "../db.ts";
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
        if (!record.instanceId) throw invalid("records[].instance_id is required");
        const start = time(record.startTime, "records[].start_time");
        const end = time(record.endTime, "records[].end_time");
        if (end < start) throw invalid("records[].end_time is before start_time");
        return {
          id: record.id,
          instance_id: record.instanceId,
          start_time: start,
          end_time: end,
          player_seconds: record.playerSeconds.toString(),
        };
      });
      // Each span is cut to the time its instance owned the environment, on management's clock, so neither a core that
      // hasn't heard of its successor yet nor clocks that disagree make spans overlap. A span its instance never owned
      // any of is dropped. Holding the environment's row orders this with takeovers, which cut stored spans.
      await sql.begin(async (tx) => {
        await tx`select 1 from environments where id = ${environmentId} for share`;
        await tx`
          insert into usage_records (environment_id, id, instance_id, start_time, end_time, player_seconds)
          select ${environmentId}, id, instance_id, cut_start, cut_end,
            case when cut_end - cut_start = end_time - start_time then player_seconds
              else floor(player_seconds * extract(epoch from cut_end - cut_start)
                / extract(epoch from end_time - start_time))::bigint
            end
          from (
            select r.*, greatest(r.start_time, o.owned_since) as cut_start, least(r.end_time, o.superseded_time) as cut_end
            from jsonb_to_recordset(${JSON.stringify(records)}::text::jsonb)
              as r(id text, instance_id text, start_time timestamptz, end_time timestamptz, player_seconds bigint)
            join (
              select owner_instance_id as instance_id, owner_since as owned_since, null::timestamptz as superseded_time
              from environments where id = ${environmentId}
              union all
              select instance_id, owned_since, superseded_time from superseded_instances
              where environment_id = ${environmentId}
            ) o using (instance_id)
          ) r
          where cut_end > cut_start
          on conflict do nothing`;
      });
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

/**
 * Ends the instance's stored spans at the takeover that superseded it, now, and drops those that start later, as
 * `reportUsage` cuts its later reports.
 */
export async function endUsage(db: Db, environmentId: string, instanceId: string): Promise<void> {
  await db`
    delete from usage_records
    where environment_id = ${environmentId} and instance_id = ${instanceId} and start_time >= now()`;
  await db`
    update usage_records
    set end_time = now(),
      player_seconds = floor(player_seconds * extract(epoch from now() - start_time)
        / extract(epoch from end_time - start_time))::bigint
    where environment_id = ${environmentId} and instance_id = ${instanceId} and end_time > now()`;
}

function batch<T>(items: T[], field: string): T[] {
  if (items.length > maxBatch) throw invalid(`at most ${maxBatch} ${field} are allowed per call`);
  return items;
}

function time(value: Timestamp | undefined, field: string): string {
  if (!value) throw invalid(`${field} is required`);
  return timestampDate(value).toISOString();
}
