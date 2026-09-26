// @vitest-environment happy-dom
import { Code, ConnectError } from "@connectrpc/connect";
import { beforeEach, expect, test } from "vitest";

import { bearer } from "./client.ts";
import { getToken, setToken } from "./session.ts";

type Next = Parameters<typeof bearer>[0];
const request = () => ({ header: new Headers() }) as Parameters<ReturnType<typeof bearer>>[0];
const unauthenticated = () => new ConnectError("a valid bearer token is required", Code.Unauthenticated);

beforeEach(() => setToken("first"));

test("a rejected token from the current session signs out", async () => {
  const next: Next = async () => {
    throw unauthenticated();
  };
  await expect(bearer(next)(request())).rejects.toThrow();
  expect(getToken()).toBeNull();
});

test("a rejection that arrives after signing in again keeps the new session", async () => {
  let reject: (reason: unknown) => void = () => {};
  const next: Next = () => new Promise((_, fail) => (reject = fail));
  const pending = bearer(next)(request());
  setToken("second");
  reject(unauthenticated());
  await expect(pending).rejects.toThrow();
  expect(getToken()).toBe("second");
});

test("a rejection while reading a stream signs out", async () => {
  async function* messages() {
    yield {};
    throw unauthenticated();
  }
  const next = (async () => ({ stream: true, message: messages() })) as unknown as Next;
  const response = await bearer(next)(request());
  if (!response.stream) throw new Error("expected a stream");
  const read = async () => {
    for await (const _ of response.message);
  };
  await expect(read()).rejects.toThrow();
  expect(getToken()).toBeNull();
});
