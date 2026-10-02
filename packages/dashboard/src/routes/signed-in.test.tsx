// @vitest-environment happy-dom
import { create } from "@bufbuild/protobuf";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

import { GetCurrentPrincipalResponseSchema } from "../gen/chunk/management/v1/auth_pb.ts";
import { ListProjectsResponseSchema } from "../gen/chunk/management/v1/projects_pb.ts";
import { api, queryClient } from "../lib/client.ts";
import { getToken, setToken } from "../lib/session.ts";
import { routeTree } from "../router.tsx";

beforeEach(() => {
  setToken(null);
  vi.spyOn(api.auth, "getCurrentPrincipal").mockResolvedValue(create(GetCurrentPrincipalResponseSchema));
  vi.spyOn(api.projects, "listProjects").mockResolvedValue(create(ListProjectsResponseSchema));
});
afterEach(cleanup);

function open(path: string) {
  const router = createRouter({ routeTree, history: createMemoryHistory({ initialEntries: [path] }) });
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return router;
}

test("keeps a checked token and returns to the page the sign-in started from", async () => {
  const router = open(`/signed-in?return=${encodeURIComponent("/login?code=BCDF-GHJK")}#token=chunk_valid`);

  await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
  expect(router.state.location.search).toEqual({ code: "BCDF-GHJK" });
  expect(getToken()).toBe("chunk_valid");
  expect(vi.mocked(api.auth.getCurrentPrincipal)).toHaveBeenCalledWith(
    {},
    { headers: { authorization: "Bearer chunk_valid" } },
  );
});

test("goes to the projects instead of a return path on another origin", async () => {
  const router = open(`/signed-in?return=${encodeURIComponent("//elsewhere.example/steal")}#token=chunk_valid`);

  await waitFor(() => expect(router.state.location.href).toBe("/"));
  expect(getToken()).toBe("chunk_valid");
});
