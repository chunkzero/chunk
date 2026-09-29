interface Run {
  work: () => Promise<void>;
  done: Promise<void>;
  settle(error?: unknown): void;
}

function run(work: () => Promise<void>): Run {
  let settle!: (error?: unknown) => void;
  const done = new Promise<void>((resolve, reject) => {
    settle = (error) => (error === undefined ? resolve() : reject(error));
  });
  return { work, done, settle };
}

/**
 * Runs work by key: at most `concurrency` runs at once, and at most one per key. Work scheduled for a key whose run has
 * not started yet joins that run; work scheduled while it is under way gets one more run after it, which every later
 * request joins until it starts. Each returned promise settles with the run that serves it, so that run starts after
 * the request.
 */
export function keyedPool(concurrency: number) {
  const keys = new Map<string, { current: Run; started: boolean; next: Run | undefined }>();
  const loops = new Set<Promise<void>>();
  const waiting: (() => void)[] = [];
  let free = concurrency;

  const acquire = async () => {
    if (free > 0) free--;
    else await new Promise<void>((resolve) => waiting.push(resolve));
  };
  const release = () => {
    const next = waiting.shift();
    if (next) next();
    else free++;
  };

  async function drain(key: string) {
    for (let entry = keys.get(key); entry;) {
      await acquire();
      entry.started = true;
      try {
        await entry.current.work();
        entry.current.settle();
      } catch (error) {
        entry.current.settle(error ?? new Error("the run failed"));
      } finally {
        release();
      }
      if (!entry.next) break;
      entry = { current: entry.next, started: false, next: undefined };
      keys.set(key, entry);
    }
    keys.delete(key);
  }

  return {
    schedule(key: string, work: () => Promise<void>): Promise<void> {
      const entry = keys.get(key);
      if (!entry) {
        const first = run(work);
        keys.set(key, { current: first, started: false, next: undefined });
        const loop = drain(key).finally(() => loops.delete(loop));
        loops.add(loop);
        return first.done;
      }
      // The latest work replaces what a pending run was given; it carries the latest leader epoch.
      const pending = entry.started ? (entry.next ??= run(work)) : entry.current;
      pending.work = work;
      return pending.done;
    },
    /** Resolves once nothing is scheduled or under way. */
    async idle(): Promise<void> {
      while (loops.size > 0) await Promise.allSettled([...loops]);
    },
  };
}

/** The first retry waits this long, doubling up to `maxDelayMs`. */
const firstDelayMs = 1000;
const maxDelayMs = 30_000;

/**
 * Tracks transient failures by key, such as a provider with no room or a call that timed out, so retries back off. A
 * key's retries never wait past the bound, so the one that finds the failures have lasted that long comes on time.
 */
export function retryBackoff(boundMs: number, now = Date.now) {
  const keys = new Map<string, { since: number; delay: number; next: number }>();
  return {
    /** Whether `key` is backing off now. */
    waiting(key: string): boolean {
      const entry = keys.get(key);
      return entry !== undefined && now() < entry.next;
    },
    /** Records a transient failure; true once failures have lasted `boundMs`. */
    failed(key: string): boolean {
      const at = now();
      const previous = keys.get(key);
      const since = previous?.since ?? at;
      const delay = previous ? Math.min(previous.delay * 2, maxDelayMs) : firstDelayMs;
      const untilBound = since + boundMs - at;
      keys.set(key, { since, delay, next: at + (untilBound > 0 ? Math.min(delay, untilBound) : delay) });
      return untilBound <= 0;
    },
    /** Forgets `key`'s failures, once a step succeeded or failed for good. */
    clear(key: string): void {
      keys.delete(key);
    },
  };
}

export type RetryBackoff = ReturnType<typeof retryBackoff>;
