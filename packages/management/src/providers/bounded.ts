import type { Provider } from "./provider.ts";

/** How long the reconciler waits for provider calls, in milliseconds. */
export interface ProviderTimeouts {
  /** For `create` and `start`. */
  startMs: number;
  /** For every other call. */
  callMs: number;
}

/**
 * A provider call was given up on, by the reconciler's bound or by a provider's own wait; it may still finish later, so
 * nothing about its outcome is known. The reconciler retries it like a transient failure.
 */
export class ProviderTimeoutError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProviderTimeoutError";
  }
}

/**
 * Wraps `provider` so each call fails with `ProviderTimeoutError` once it runs past its bound, or `signal` aborts. The
 * call itself carries on, since a provider cannot recall it; once `signal` has aborted, no new call is made.
 */
export function boundedProvider(provider: Provider, timeouts: ProviderTimeouts, signal?: AbortSignal): Provider {
  const bound = async <T>(method: string, ms: number, call: () => Promise<T>) => {
    const abandoned = () => new ProviderTimeoutError(`the provider's ${method} was abandoned on shutdown`);
    if (signal?.aborted) throw abandoned();
    const gaveUp = Promise.withResolvers<never>();
    const timer = setTimeout(
      () => gaveUp.reject(new ProviderTimeoutError(`the provider's ${method} did not finish within ${ms} ms`)),
      ms,
    );
    const aborted = () => gaveUp.reject(abandoned());
    signal?.addEventListener("abort", aborted, { once: true });
    try {
      return await Promise.race([call(), gaveUp.promise]);
    } finally {
      clearTimeout(timer);
      signal?.removeEventListener("abort", aborted);
    }
  };
  const { startMs, callMs } = timeouts;
  return {
    create: (spec) => bound("create", startMs, () => provider.create(spec)),
    start: (id) => bound("start", startMs, () => provider.start(id)),
    suspend: (id) => bound("suspend", callMs, () => provider.suspend(id)),
    stop: (id) => bound("stop", callMs, () => provider.stop(id)),
    status: (id) => bound("status", callMs, () => provider.status(id)),
    find: (name) => bound("find", callMs, () => provider.find(name)),
    destroy: (name, options) => bound("destroy", callMs, () => provider.destroy(name, options)),
    list: () => bound("list", callMs, () => provider.list()),
  };
}
