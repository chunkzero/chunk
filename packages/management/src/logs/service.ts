import { create } from "@bufbuild/protobuf";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import type { ServiceImpl } from "@connectrpc/connect";

import type { Deps } from "../deps.ts";
import type { LogSeverity, LogSource } from "../gen/chunk/management/v1/common_pb.ts";
import { type LogService, ReadLogsResponseSchema } from "../gen/chunk/management/v1/logs_pb.ts";
import { loadEnvironment } from "../projects/store.ts";
import { callerOf } from "../rpc/caller.ts";
import { timestamp } from "../rpc/validate.ts";

interface LogRow {
  seq: bigint;
  instance_id: string;
  sequence: bigint;
  time: Date;
  source: LogSource;
  severity: LogSeverity;
  message: string;
  app_id: string;
  deployment_id: string;
}

const defaultLimit = 1000;
const maxLimit = 10_000;
const batchSize = 500;
const keepaliveMs = 30_000;

export function logService({ sql, changes }: Deps): Partial<ServiceImpl<typeof LogService>> {
  return {
    /** Entries come in the order the service stored them. */
    async *readLogs(request, context) {
      const environment = await loadEnvironment(sql, callerOf(context), request.environmentId);
      const limit = Math.min(request.limit || defaultLimit, maxLimit);
      const subscription = request.follow
        ? changes.subscribe((change) => change.kind === "logs" && change.environmentId === environment.id)
        : undefined;
      try {
        const matching = sql`
          environment_id = ${environment.id}
          ${request.deploymentId ? sql`and deployment_id = ${request.deploymentId}` : sql``}
          ${request.appId ? sql`and app_id = ${request.appId}` : sql``}`;
        // Following resumes after the newest entry that existed when the stored ones were read, so none repeats.
        const [{ last } = { last: 0n }] = await sql<{ last: bigint }[]>`
          select coalesce(max(seq), 0) as last from log_entries where environment_id = ${environment.id}`;
        const stored = request.startTime
          ? await sql<LogRow[]>`
              select * from log_entries
              where ${matching} and seq <= ${last} and time >= ${timestampDate(request.startTime)}
              order by seq limit ${limit}`
          : (
              await sql<LogRow[]>`
                select * from log_entries where ${matching} and seq <= ${last} order by seq desc limit ${limit}`
            ).reverse();
        for (let start = 0; start < stored.length; start += batchSize) {
          yield response(stored.slice(start, start + batchSize));
        }
        if (!subscription) return;

        let after = last;
        let sentAt = Date.now();
        while (!context.signal.aborted) {
          await subscription.next(keepaliveMs - (Date.now() - sentAt), context.signal);
          if (context.signal.aborted) break;
          let rows: LogRow[];
          do {
            rows = await sql<LogRow[]>`
              select * from log_entries where ${matching} and seq > ${after} order by seq limit ${batchSize}`;
            after = rows.at(-1)?.seq ?? after;
            // An empty batch is the keepalive.
            if (rows.length > 0 || Date.now() - sentAt >= keepaliveMs) {
              yield response(rows);
              sentAt = Date.now();
            }
          } while (rows.length === batchSize);
        }
      } finally {
        subscription?.close();
      }
    },
  };
}

function response(rows: LogRow[]) {
  return create(ReadLogsResponseSchema, {
    entries: rows.map((row) => ({
      time: timestamp(row.time),
      source: row.source,
      severity: row.severity,
      message: row.message,
      instanceId: row.instance_id,
      appId: row.app_id,
      deploymentId: row.deployment_id,
      sequence: row.sequence,
    })),
  });
}
