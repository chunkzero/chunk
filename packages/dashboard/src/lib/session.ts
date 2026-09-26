import { useSyncExternalStore } from "react";

const key = "chunk.token";
const listeners = new Set<() => void>();

/** The API token lives in sessionStorage, so it lasts for this tab only. */
export function getToken() {
  return sessionStorage.getItem(key);
}

export function setToken(token: string | null) {
  if (token === null) sessionStorage.removeItem(key);
  else sessionStorage.setItem(key, token);
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useToken() {
  return useSyncExternalStore(subscribe, getToken);
}
