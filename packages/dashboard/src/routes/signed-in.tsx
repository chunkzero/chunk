import * as stylex from "@stylexjs/stylex";
import { redirect, useRouter } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";

import { Logo } from "../components/logo.tsx";
import { ErrorText } from "../components/page.tsx";
import { Button } from "../components/ui/button.tsx";
import { api, errorMessage } from "../lib/client.ts";
import { finishSignIn, setToken } from "../lib/session.ts";
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

interface Handoff {
  token: string | null;
  /** Whether this tab started the sign-in, by the `state` it handed out. */
  solicited: boolean;
  target: string;
}

/** The handoff the address bar last carried, until `SignedIn` takes it. */
let pending: Handoff | undefined;

/**
 * Where an install's sign-in flow ends: `/signed-in?return=<path>#token=<API token>&state=<state>`. Takes the handoff
 * out of the address bar and history before anything else, by redirecting to a bare `/signed-in`.
 */
export function takeHandoff(hash: string, back: string | undefined) {
  if (!hash) return;
  const params = new URLSearchParams(hash);
  pending = {
    token: params.get("token"),
    solicited: finishSignIn(params.get("state")),
    target: sameOriginPath(back) ?? "/",
  };
  throw redirect({ to: "/signed-in", replace: true });
}

/** Checks the handed-off token, keeps it for this tab and goes on to the page the sign-in started from. */
export function SignedIn() {
  const router = useRouter();
  const started = useRef(false);
  const [error, setError] = useState<string>();

  useEffect(() => {
    if (started.current) return;
    started.current = true;
    const handoff = pending;
    pending = undefined;
    if (!handoff?.token) {
      setError("The sign-in link carried no token.");
    } else if (!handoff.solicited) {
      setError("This sign-in wasn't started from this tab. Start it again from the sign-in page.");
    } else {
      const { token, target } = handoff;
      api.auth.getCurrentPrincipal({}, { headers: { authorization: `Bearer ${token}` } }).then(
        () => {
          setToken(token);
          router.history.replace(target);
        },
        (failure: unknown) => setError(errorMessage(failure)),
      );
    }
  }, [router]);

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
  try {
    const url = new URL(path, window.location.origin);
    return url.origin === window.location.origin ? `${url.pathname}${url.search}${url.hash}` : undefined;
  } catch {
    return undefined;
  }
}
