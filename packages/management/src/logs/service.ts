import { create } from "@bufbuild/protobuf";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import type { ServiceImpl } from "@connectrpc/connect";
import { and, asc, desc, eq, gt, gte, lte, sql } from "drizzle-orm";

import type { Deps } from "../deps.ts";
import { type LogService, ReadLogsResponseSchema } from "../gen/chunk/management/v1/logs_pb.ts";
import { loadEnvironment } from "../projects/store.ts";
import { callerOf } from "../rpc/caller.ts";
import { endIfShuttingDown, streamSignal, timestamp } from "../rpc/validate.ts";
import { logEntries } from "../schema.ts";

type LogRow = typeof logEntries.$inferSelect;

const defaultLimit = 1000;
const maxLimit = 10_000;
const batchSize = 500;
const keepaliveMs = 30_000;

export function logService({ db, changes, shutdown }: Deps): Partial<ServiceImpl<typeof LogService>> {
  return {
    /** Entries come in the order the service stored them. */
    async *readLogs(request, context) {
      const environment = await loadEnvironment(db, callerOf(context), request.environmentId);
      const limit = Math.min(request.limit || defaultLimit, maxLimit);
      const subscription = request.follow
        ? changes.subscribe((change) => change.kind === "logs" && change.environmentId === environment.id)
        : undefined;
      try {
        const matching = and(
          eq(logEntries.environment_id, environment.id),
          request.deploymentId ? eq(logEntries.deployment_id, request.deploymentId) : undefined,
          request.appId ? eq(logEntries.app_id, request.appId) : undefined,
          request.startTime ? gte(logEntries.time, timestampDate(request.startTime)) : undefined,
        );
        // Following resumes after the newest entry that existed when the stored ones were read, so none repeats.
        const [{ last } = { last: 0n }] = await db
          .select({ last: sql`coalesce(max(${logEntries.seq}), 0)`.mapWith(logEntries.seq) })
          .from(logEntries)
          .where(eq(logEntries.environment_id, environment.id));
        const upToLast = and(matching, lte(logEntries.seq, last));
        const stored = request.startTime
          ? await db.select().from(logEntries).where(upToLast).orderBy(asc(logEntries.seq)).limit(limit)
          : (await db.select().from(logEntries).where(upToLast).orderBy(desc(logEntries.seq)).limit(limit)).reverse();
        for (let start = 0; start < stored.length; start += batchSize) {
          yield response(stored.slice(start, start + batchSize));
        }
        if (!subscription) return;

        let after = last;
        let sentAt = Date.now();
        const signal = streamSignal(context.signal, shutdown);
        while (!signal.aborted) {
          await subscription.next(keepaliveMs - (Date.now() - sentAt), signal);
          if (signal.aborted) break;
          let rows: LogRow[];
          do {
            rows = await db
              .select()
              .from(logEntries)
              .where(and(matching, gt(logEntries.seq, after)))
              .orderBy(asc(logEntries.seq))
              .limit(batchSize);
            after = rows.at(-1)?.seq ?? after;
            // An empty batch is the keepalive.
            if (rows.length > 0 || Date.now() - sentAt >= keepaliveMs) {
              yield response(rows);
              sentAt = Date.now();
            }
          } while (rows.length === batchSize);
        }
        endIfShuttingDown(shutdown);
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
