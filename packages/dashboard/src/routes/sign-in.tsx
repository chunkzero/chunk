import * as stylex from "@stylexjs/stylex";
import { useMutation } from "@tanstack/react-query";
import { useState } from "react";

import { Logo } from "../components/logo.tsx";
import { ErrorText } from "../components/page.tsx";
import { Button } from "../components/ui/button.tsx";
import { Input } from "../components/ui/input.tsx";
import { Label } from "../components/ui/label.tsx";
import { api, errorMessage } from "../lib/client.ts";
import { setToken } from "../lib/session.ts";
import { colors, fonts, fontSizes, lineHeights, space } from "../tokens.stylex.ts";

const styles = stylex.create({
  screen: { display: "flex", alignItems: "center", justifyContent: "center", minHeight: "100svh", padding: space.s6 },
  form: { display: "flex", flexDirection: "column", gap: space.s5, width: "100%", maxWidth: "20rem" },
  field: { display: "flex", flexDirection: "column", gap: space.s2 },
  token: { fontFamily: fonts.mono },
  hint: { fontSize: fontSizes.xs, lineHeight: lineHeights.xs, color: colors.mutedForeground },
  submit: { width: "100%" },
});

export function SignIn() {
  const [token, setCandidate] = useState("");
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
