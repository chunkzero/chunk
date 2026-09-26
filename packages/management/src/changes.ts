import type { Db, Sql } from "./db.ts";

const channel = "chunk_management_changes";

/** What changed: an environment's desired state, status or routes, or its stored logs. */
export interface Change {
  kind: "environment" | "logs";
  environmentId: string;
}

/** Wakes streams when data they serve changes. */
export interface Changes {
  /** Starts collecting matching changes; subscribe before reading, so nothing between the two is missed. */
  subscribe(matches: (change: Change) => boolean): Subscription;
}

export interface Subscription {
  /** Resolves once a matching change arrived since the last call, after `timeoutMs`, or when `signal` aborts. */
  next(timeoutMs: number, signal: AbortSignal): Promise<void>;
  close(): void;
}

/** Announces a change. Inside a transaction, Postgres delivers it only when the transaction commits. */
export async function notify(db: Db, change: Change): Promise<void> {
  await db`select pg_notify(${channel}, ${`${change.kind}:${change.environmentId}`})`;
}

/** Listens for changes from every process sharing the database. */
export async function listenForChanges(sql: Sql): Promise<Changes> {
  const subscribers = new Set<{ matches: (change: Change) => boolean; signal: () => void }>();
  await sql.listen(channel, (payload) => {
    const [kind, environmentId = ""] = payload.split(":");
    if (kind !== "environment" && kind !== "logs") return;
    for (const subscriber of subscribers) {
      if (subscriber.matches({ kind, environmentId })) subscriber.signal();
    }
  });
  return {
    subscribe(matches) {
      let pending = false;
      let wake: (() => void) | undefined;
      const subscriber = {
        matches,
        signal() {
          pending = true;
          wake?.();
        },
      };
      subscribers.add(subscriber);
      return {
        async next(timeoutMs, signal) {
          if (!pending && !signal.aborted) {
            await new Promise<void>((resolve) => {
              const done = () => {
                clearTimeout(timer);
                signal.removeEventListener("abort", done);
                resolve();
              };
              const timer = setTimeout(done, timeoutMs);
              signal.addEventListener("abort", done);
              wake = done;
            });
          }
          wake = undefined;
          pending = false;
        },
        close() {
          subscribers.delete(subscriber);
        },
      };
    },
  };
}
