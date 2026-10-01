/** Settles like `work`, or rejects once `signal` aborts, whether or not `work` stops then. */
export async function untilAborted<T>(signal: AbortSignal, work: Promise<T>): Promise<T> {
  const aborted = Promise.withResolvers<never>();
  const abort = () => aborted.reject(signal.reason);
  if (signal.aborted) abort();
  signal.addEventListener("abort", abort, { once: true });
  try {
    return await Promise.race([work, aborted.promise]);
  } finally {
    signal.removeEventListener("abort", abort);
  }
}
