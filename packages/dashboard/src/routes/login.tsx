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

/** Approves a `chunk login` device code; the CLI links here with `?code=`. */
export function Login() {
  const { code } = useSearch({ from: "/login" });
  return (
    <Page>
      <Panel title="Approve a CLI login" className="mx-auto max-w-md">
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
      <p className="px-5 py-6 text-sm">
        Approved <span className="font-mono">{userCode}</span>. The CLI finishes signing in on its own; you can close
        this tab.
      </p>
    );
  }
  return (
    <form
      className="space-y-5 px-5 py-6"
      onSubmit={(event) => {
        event.preventDefault();
        approve.mutate();
      }}
    >
      {code === undefined ? (
        <div className="space-y-2">
          <Label htmlFor="code">Code from your terminal</Label>
          <Input
            id="code"
            required
            autoFocus
            autoComplete="off"
            className="font-mono tracking-widest uppercase"
            value={typed}
            onChange={(event) => setTyped(event.target.value)}
          />
        </div>
      ) : (
        <div className="space-y-2">
          <p className="text-sm text-muted-foreground">Check that your terminal shows this code.</p>
          <p className="rounded-md border bg-background py-4 text-center font-mono text-2xl tracking-[0.2em]">
            {userCode}
          </p>
        </div>
      )}
      <p className="text-xs text-muted-foreground">
        The CLI gets its own token acting as {principal.data?.principal?.displayName ?? "you"}.
      </p>
      <ErrorText error={approve.error ? errorMessage(approve.error) : undefined} />
      <Button type="submit" className="w-full" disabled={!userCode || approve.isPending}>
        {approve.isPending ? "Approving…" : "Approve"}
      </Button>
    </form>
  );
}
