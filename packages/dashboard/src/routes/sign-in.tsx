import * as stylex from "@stylexjs/stylex";
import { useMutation, useQuery } from "@tanstack/react-query";
import { useLocation } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { Logo } from "../components/logo.tsx";
import { ErrorText } from "../components/page.tsx";
import { Button } from "../components/ui/button.tsx";
import { Input } from "../components/ui/input.tsx";
import { Label } from "../components/ui/label.tsx";
import { api, errorMessage } from "../lib/client.ts";
import { setToken, startSignIn } from "../lib/session.ts";
import { colors, fonts, fontSizes, lineHeights, space } from "../tokens.stylex.ts";

const styles = stylex.create({
  screen: { display: "flex", alignItems: "center", justifyContent: "center", minHeight: "100svh", padding: space.s6 },
  form: { display: "flex", flexDirection: "column", gap: space.s5, width: "100%", maxWidth: "20rem" },
  field: { display: "flex", flexDirection: "column", gap: space.s2 },
  options: { display: "flex", flexDirection: "column", gap: space.s2 },
  token: { fontFamily: fonts.mono },
  hint: { fontSize: fontSizes.xs, lineHeight: lineHeights.xs, color: colors.mutedForeground },
  submit: { width: "100%" },
});

export function SignIn() {
  const [token, setCandidate] = useState("");
  const here = useLocation({ select: (location) => `${location.pathname}${location.searchStr}` });
  const options = useQuery({
    queryKey: ["sign-in-options"],
    queryFn: async () => (await api.auth.getSignInOptions({})).options,
    staleTime: Infinity,
  });
  const offered = (options.data?.length ?? 0) > 0;
  const [state, setState] = useState<string>();
  useEffect(() => {
    if (offered) setState(startSignIn());
  }, [offered]);
  const attempt = useMutation({
    mutationFn: (candidate: string) =>
      api.auth.getCurrentPrincipal({}, { headers: { authorization: `Bearer ${candidate}` } }),
    onSuccess: (_, candidate) => setToken(candidate),
  });

  return (
    <div {...stylex.props(styles.screen)}>
      <form
        {...stylex.props(styles.form)}
        onSubmit={(event) => {
          event.preventDefault();
          attempt.mutate(token.trim());
        }}
      >
        <Logo />
        {state && (
          <div {...stylex.props(styles.options)}>
            {options.data?.map((option) => (
              <Button key={option.url} variant="outline" href={handoff(option.url, here, state)} style={styles.submit}>
                {option.label}
              </Button>
            ))}
          </div>
        )}
        <div {...stylex.props(styles.field)}>
          <Label htmlFor="token">API token</Label>
          <Input
            id="token"
            type="password"
            required
            autoComplete="off"
            autoFocus
            style={styles.token}
            value={token}
            onValueChange={setCandidate}
            aria-invalid={attempt.isError}
          />
          <p {...stylex.props(styles.hint)}>
            An API token for {window.location.host}, such as its CHUNK_OPERATOR_TOKEN. Kept for this tab only.
          </p>
        </div>
        <ErrorText error={attempt.error ? errorMessage(attempt.error) : undefined} />
        <Button type="submit" style={styles.submit} disabled={!token.trim() || attempt.isPending}>
          {attempt.isPending ? "Checking…" : "Continue"}
        </Button>
      </form>
    </div>
  );
}

/** `url` with the dashboard path to come back to and the sign-in's `state`, which the flow echoes back. */
function handoff(url: string, path: string, state: string) {
  const target = new URL(url, window.location.origin);
  target.searchParams.set("return", path);
  target.searchParams.set("state", state);
  return target.href;
}
