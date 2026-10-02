import * as stylex from "@stylexjs/stylex";
import { useLocation, useRouter, useSearch } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { Logo } from "../components/logo.tsx";
import { ErrorText } from "../components/page.tsx";
import { Button } from "../components/ui/button.tsx";
import { api, errorMessage } from "../lib/client.ts";
import { setToken } from "../lib/session.ts";
import { colors, fontSizes, lineHeights, space } from "../tokens.stylex.ts";

const styles = stylex.create({
  screen: {
    display: "flex",
    flexDirection: "column",
    alignItems: "center",
    justifyContent: "center",
    gap: space.s5,
    minHeight: "100svh",
    padding: space.s6,
  },
  status: { fontSize: fontSizes.sm, lineHeight: lineHeights.sm, color: colors.mutedForeground },
});

/**
 * Where an install's sign-in flow ends: `/signed-in?return=<path>#token=<API token>`. Checks the token, keeps it for
 * this tab and goes on to `return` when that is a path on this origin, or to the projects otherwise.
 */
export function SignedIn() {
  const router = useRouter();
  const back = useSearch({ from: "/signed-in", select: (search) => search.return });
  const token = useLocation({ select: (location) => new URLSearchParams(location.hash).get("token") });
  const [error, setError] = useState<string>();

  useEffect(() => {
    if (!token) {
      setError("The sign-in link carried no token.");
      return;
    }
    let current = true;
    api.auth.getCurrentPrincipal({}, { headers: { authorization: `Bearer ${token}` } }).then(
      () => {
        if (!current) return;
        setToken(token);
        // Replacing this entry takes the token out of the address bar and history.
        router.history.replace(sameOriginPath(back) ?? "/");
      },
      (failure: unknown) => current && setError(errorMessage(failure)),
    );
    return () => {
      current = false;
    };
  }, [token, back, router]);

  return (
    <div {...stylex.props(styles.screen)}>
      <Logo />
      {error ? (
        <>
          <ErrorText error={`Signing in failed: ${error}`} />
          <Button variant="outline" href="/">
            Back to sign in
          </Button>
        </>
      ) : (
        <p {...stylex.props(styles.status)}>Signing in…</p>
      )}
    </div>
  );
}

/** `path` when it is a path on this origin, such as `/p/prj_1?tab=logs`; undefined for anything else. */
function sameOriginPath(path: string | undefined): string | undefined {
  if (!path?.startsWith("/")) return undefined;
  const url = new URL(path, window.location.origin);
  return url.origin === window.location.origin ? `${url.pathname}${url.search}${url.hash}` : undefined;
}
