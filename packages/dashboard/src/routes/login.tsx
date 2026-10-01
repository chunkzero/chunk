import * as stylex from "@stylexjs/stylex";
import { useMutation } from "@tanstack/react-query";
import { useSearch } from "@tanstack/react-router";
import { useState } from "react";

import { ErrorText, Page } from "../components/page.tsx";
import { Panel } from "../components/panel.tsx";
import { Button } from "../components/ui/button.tsx";
import { Input } from "../components/ui/input.tsx";
import { Label } from "../components/ui/label.tsx";
import { api, errorMessage } from "../lib/client.ts";
import { usePrincipal } from "../lib/queries.ts";
import { colors, fonts, fontSizes, lineHeights, radii, space } from "../tokens.stylex.ts";

const styles = stylex.create({
  panel: { width: "100%", maxWidth: "28rem", marginInline: "auto" },
  approved: { paddingInline: space.s5, paddingBlock: space.s6, fontSize: fontSizes.sm, lineHeight: lineHeights.sm },
  mono: { fontFamily: fonts.mono },
  form: { display: "flex", flexDirection: "column", gap: space.s5, paddingInline: space.s5, paddingBlock: space.s6 },
  field: { display: "flex", flexDirection: "column", gap: space.s2 },
  typed: { fontFamily: fonts.mono, letterSpacing: "0.1em", textTransform: "uppercase" },
  hint: { fontSize: fontSizes.sm, lineHeight: lineHeights.sm, color: colors.mutedForeground },
  code: {
    paddingBlock: space.s4,
    borderWidth: "1px",
    borderStyle: "solid",
    borderColor: colors.border,
    borderRadius: radii.md,
    backgroundColor: colors.background,
    textAlign: "center",
    fontFamily: fonts.mono,
    fontSize: fontSizes.xxl,
    lineHeight: lineHeights.xxl,
    letterSpacing: "0.2em",
  },
  note: { fontSize: fontSizes.xs, lineHeight: lineHeights.xs, color: colors.mutedForeground },
  submit: { width: "100%" },
});

/** Approves a `chunk login` device code; the CLI links here with `?code=`. */
export function Login() {
  const { code } = useSearch({ from: "/login" });
  return (
    <Page>
      <Panel title="Approve a CLI login" style={styles.panel}>
        <Approval key={code ?? ""} code={code} />
      </Panel>
    </Page>
  );
}

/** Keyed by the linked code, so a different code starts over instead of inheriting an earlier approval. */
function Approval({ code }: { code: string | undefined }) {
  const [typed, setTyped] = useState("");
  const userCode = (code ?? typed).trim();
  const principal = usePrincipal();
  const approve = useMutation({ mutationFn: () => api.auth.approveLogin({ userCode }) });

  if (approve.isSuccess) {
    return (
      <p {...stylex.props(styles.approved)}>
        Approved <span {...stylex.props(styles.mono)}>{userCode}</span>. The CLI finishes signing in on its own; you can
        close this tab.
      </p>
    );
  }
  return (
    <form
      {...stylex.props(styles.form)}
      onSubmit={(event) => {
        event.preventDefault();
        approve.mutate();
      }}
    >
      {code === undefined ? (
        <div {...stylex.props(styles.field)}>
          <Label htmlFor="code">Code from your terminal</Label>
          <Input
            id="code"
            required
            autoFocus
            autoComplete="off"
            style={styles.typed}
            value={typed}
            onValueChange={setTyped}
          />
        </div>
      ) : (
        <div {...stylex.props(styles.field)}>
          <p {...stylex.props(styles.hint)}>Check that your terminal shows this code.</p>
          <p {...stylex.props(styles.code)}>{userCode}</p>
        </div>
      )}
      <p {...stylex.props(styles.note)}>
        The CLI gets its own token acting as {principal.data?.principal?.displayName ?? "you"}.
      </p>
      <ErrorText error={approve.error ? errorMessage(approve.error) : undefined} />
      <Button type="submit" style={styles.submit} disabled={!userCode || approve.isPending}>
        {approve.isPending ? "Approving…" : "Approve"}
      </Button>
    </form>
  );
}
