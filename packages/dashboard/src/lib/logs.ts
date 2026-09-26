import { useEffect, useState } from "react";

import type { LogEntry } from "../gen/chunk/management/v1/common_pb.ts";
import { api, errorMessage } from "./client.ts";

const keep = 5000;

export interface LogStream {
  entries: LogEntry[];
  loading: boolean;
  error: string | undefined;
  retry: () => void;
}

/** Streams an environment's recent log entries, and new ones while `follow` is on. */
export function useLogStream(environmentId: string, follow: boolean): LogStream {
  const [entries, setEntries] = useState<LogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    const controller = new AbortController();
    setEntries([]);
    setLoading(true);
    setError(undefined);
    void (async () => {
      try {
        const stream = api.logs.readLogs({ environmentId, follow }, { signal: controller.signal });
        for await (const response of stream) {
          setLoading(false);
          if (response.entries.length > 0) setEntries((current) => [...current, ...response.entries].slice(-keep));
        }
      } catch (caught) {
        if (!controller.signal.aborted) setError(errorMessage(caught));
      } finally {
        if (!controller.signal.aborted) setLoading(false);
      }
    })();
    return () => controller.abort();
  }, [environmentId, follow, attempt]);

  return { entries, loading, error, retry: () => setAttempt((value) => value + 1) };
}
