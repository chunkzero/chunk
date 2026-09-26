// @vitest-environment happy-dom
import { create } from "@bufbuild/protobuf";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { expect, test, vi } from "vitest";

import { ApproveLoginResponseSchema, GetCurrentPrincipalResponseSchema } from "../gen/chunk/management/v1/auth_pb.ts";
import { api, queryClient } from "../lib/client.ts";
import { setToken } from "../lib/session.ts";
import { routeTree } from "../router.tsx";

test("a newly linked code is shown and approved instead of the earlier one", async () => {
  setToken("test-token");
  vi.spyOn(api.auth, "getCurrentPrincipal").mockResolvedValue(create(GetCurrentPrincipalResponseSchema));
  const approve = vi.spyOn(api.auth, "approveLogin").mockResolvedValue(create(ApproveLoginResponseSchema));
  const router = createRouter({
    routeTree,
    history: createMemoryHistory({ initialEntries: ["/login?code=BCDF-GHJK"] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );

  fireEvent.click(await screen.findByRole("button", { name: "Approve" }));
  await screen.findByText(/^Approved/);
  await act(() => router.navigate({ to: "/login", search: { code: "LMNP-QRST" } }));

  expect(await screen.findByText("LMNP-QRST")).toBeTruthy();
  fireEvent.click(await screen.findByRole("button", { name: "Approve" }));
  await screen.findByText(/^Approved/);
  expect(approve.mock.calls.map(([request]) => request.userCode)).toEqual(["BCDF-GHJK", "LMNP-QRST"]);
});
