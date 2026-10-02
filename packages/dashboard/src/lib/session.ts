import { useSyncExternalStore } from "react";

const key = "chunk.token";
const listeners = new Set<() => void>();
let generation = 0;

/** The API token lives in sessionStorage, so it lasts for this tab only. */
export function getToken() {
  return sessionStorage.getItem(key);
}

/** Changes whenever the token is set or cleared, so a response can tell whether it belongs to the current session. */
export function getGeneration() {
  return generation;
}

export function setToken(token: string | null) {
  if (token === null) sessionStorage.removeItem(key);
  else sessionStorage.setItem(key, token);
  generation += 1;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useToken() {
  return useSyncExternalStore(subscribe, getToken);
}

const signInStateKey = "chunk.sign-in-state";

/**
 * Starts an install's sign-in from this tab: a new single-use value its flow echoes back to /signed-in, which accepts a
 * token only along with it, so no other site can hand this tab a token of its choosing.
 */
export function startSignIn() {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  const state = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  sessionStorage.setItem(signInStateKey, state);
  return state;
}

/** Whether `state` is the one this tab started its sign-in with. Either way, it can't be used again. */
export function finishSignIn(state: string | null) {
  const expected = sessionStorage.getItem(signInStateKey);
  sessionStorage.removeItem(signInStateKey);
  return expected !== null && state === expected;
}
