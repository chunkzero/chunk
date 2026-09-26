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
