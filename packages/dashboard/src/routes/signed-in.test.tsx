// @vitest-environment happy-dom
import { create } from "@bufbuild/protobuf";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

import { GetCurrentPrincipalResponseSchema } from "../gen/chunk/management/v1/auth_pb.ts";
import { ListProjectsResponseSchema } from "../gen/chunk/management/v1/projects_pb.ts";
import { api, queryClient } from "../lib/client.ts";
import { getToken, setToken, startSignIn } from "../lib/session.ts";
import { routeTree } from "../router.tsx";

beforeEach(() => {
  setToken(null);
  vi.spyOn(api.auth, "getCurrentPrincipal").mockResolvedValue(create(GetCurrentPrincipalResponseSchema));
  vi.spyOn(api.projects, "listProjects").mockResolvedValue(create(ListProjectsResponseSchema));
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

/** Lands on /signed-in as an install's sign-in flow would, handing off `token` with `state`. */
function land(back: string, state: string, token = "chunk_valid") {
  const path = `/signed-in?return=${encodeURIComponent(back)}#token=${token}&state=${state}`;
  const router = createRouter({ routeTree, history: createMemoryHistory({ initialEntries: [path] }) });
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return router;
}

test("keeps a checked token and returns to the page the sign-in started from", async () => {
  const router = land("/login?code=BCDF-GHJK", startSignIn());

  await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
  expect(router.state.location.search).toEqual({ code: "BCDF-GHJK" });
  expect(getToken()).toBe("chunk_valid");
  expect(vi.mocked(api.auth.getCurrentPrincipal)).toHaveBeenCalledWith(
    {},
    { headers: { authorization: "Bearer chunk_valid" } },
  );
});

test("goes to the projects instead of a return path on another origin or a malformed one", async () => {
  for (const back of ["//elsewhere.example/steal", "//["]) {
    const router = land(back, startSignIn());
    await waitFor(() => expect(router.state.location.href).toBe("/"));
    expect(getToken()).toBe("chunk_valid");
    cleanup();
    setToken(null);
  }
});

test("refuses a handoff this tab didn't start, or one already used", async () => {
  const used = startSignIn();
  const first = land("/", used);
  await waitFor(() => expect(first.state.location.href).toBe("/"));
  cleanup();
  setToken(null);

  // Unsolicited, then mismatched, then the used one again.
  for (const state of ["unsolicited", (startSignIn(), "mismatched"), used]) {
    const router = land("/", state, "chunk_attacker");
    expect(await screen.findByText(/wasn't started from this tab/)).toBeTruthy();
    expect(router.state.location.href).toBe("/signed-in");
    cleanup();
  }
  expect(getToken()).toBeNull();
  expect(vi.mocked(api.auth.getCurrentPrincipal)).not.toHaveBeenCalledWith(
    {},
    { headers: { authorization: "Bearer chunk_attacker" } },
  );
});
