import type { FunctionReference } from "./functions.ts";

export type JobId = string;
export interface RawScheduler {
  runAt(at: number, path: string, args: unknown): JobId;
  cancel(id: JobId): void;
  retry(id: JobId, at: number, acknowledgePossibleEffects: boolean): void;
}
export interface Scheduler {
  runAt<A, R>(at: number, action: FunctionReference<"action", A, R>, args: A): JobId;
  cancel(id: JobId): void;
  retry(id: JobId, at: number, options: { acknowledgePossibleEffects: true }): void;
}
export function scheduler(raw: RawScheduler): Scheduler {
  return Object.freeze({
    runAt: <A, R>(at: number, action: FunctionReference<"action", A, R>, args: A) => {
      if (action.kind !== "action" || !Number.isSafeInteger(at) || at < 0)
        throw new Error("Invalid scheduled action or time");
      return raw.runAt(at, action.path, action.arguments.parse(args));
    },
    cancel: (id: JobId) => raw.cancel(id),
    retry: (id: JobId, at: number, options: { acknowledgePossibleEffects: true }) => {
      if (options.acknowledgePossibleEffects !== true || !Number.isSafeInteger(at) || at < 0)
        throw new Error("Retry requires a valid time and acknowledgement of possible effects");
      raw.retry(id, at, true);
    },
  });
}
