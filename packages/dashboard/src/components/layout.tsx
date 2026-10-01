import * as stylex from "@stylexjs/stylex";
import { Outlet } from "@tanstack/react-router";

import { useToken } from "../lib/session.ts";
import { SignIn } from "../routes/sign-in.tsx";
import { colors, fontSizes, lineHeights, space } from "../tokens.stylex.ts";
import { TopNav } from "./top-nav.tsx";

const styles = stylex.create({
  framed: { padding: space.s6, fontSize: fontSizes.sm, lineHeight: lineHeights.sm, color: colors.mutedForeground },
  shell: { display: "flex", flexDirection: "column", minHeight: "100svh" },
});

// Another site could frame the dashboard to disguise clicks on Approve, Promote or Delete.
const framed = window.self !== window.top;

export function Layout() {
  const token = useToken();
  if (framed) {
    return (
      <p {...stylex.props(styles.framed)}>
        The chunk dashboard does not run inside another page. Open {window.location.origin} directly.
      </p>
    );
  }
  if (token === null) return <SignIn />;
  return (
    <div {...stylex.props(styles.shell)}>
      <TopNav />
      <Outlet />
    </div>
  );
}
