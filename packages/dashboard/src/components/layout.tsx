import { Outlet } from "@tanstack/react-router";

import { useToken } from "../lib/session.ts";
import { SignIn } from "../routes/sign-in.tsx";
import { TopNav } from "./top-nav.tsx";

// Another site could frame the dashboard to disguise clicks on Approve, Promote or Delete.
const framed = window.self !== window.top;

export function Layout() {
  const token = useToken();
  if (framed) {
    return (
      <p className="p-6 text-sm text-muted-foreground">
        The chunk dashboard does not run inside another page. Open {window.location.origin} directly.
      </p>
    );
  }
  if (token === null) return <SignIn />;
  return (
    <div className="flex min-h-svh flex-col">
      <TopNav />
      <Outlet />
    </div>
  );
}
