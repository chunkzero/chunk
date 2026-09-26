import { Outlet } from "@tanstack/react-router";

import { useToken } from "../lib/session.ts";
import { SignIn } from "../routes/sign-in.tsx";
import { TopNav } from "./top-nav.tsx";

export function Layout() {
  if (useToken() === null) return <SignIn />;
  return (
    <div className="flex min-h-svh flex-col">
      <TopNav />
      <Outlet />
    </div>
  );
}
