import { useMutation } from "@tanstack/react-query";
import { useState } from "react";

import { Logo } from "../components/logo.tsx";
import { ErrorText } from "../components/page.tsx";
import { Button } from "../components/ui/button.tsx";
import { Input } from "../components/ui/input.tsx";
import { Label } from "../components/ui/label.tsx";
import { api, errorMessage } from "../lib/client.ts";
import { setToken } from "../lib/session.ts";

export function SignIn() {
  const [token, setCandidate] = useState("");
  const attempt = useMutation({
    mutationFn: (candidate: string) =>
      api.auth.getCurrentPrincipal({}, { headers: { authorization: `Bearer ${candidate}` } }),
    onSuccess: (_, candidate) => setToken(candidate),
  });

  return (
    <div className="flex min-h-svh items-center justify-center p-6">
      <form
        className="w-full max-w-xs space-y-5"
        onSubmit={(event) => {
          event.preventDefault();
          attempt.mutate(token.trim());
        }}
      >
        <Logo />
        <div className="space-y-2">
          <Label htmlFor="token">API token</Label>
          <Input
            id="token"
            type="password"
            required
            autoComplete="off"
            autoFocus
            className="font-mono"
            value={token}
            onChange={(event) => setCandidate(event.target.value)}
            aria-invalid={attempt.isError}
          />
          <p className="text-xs text-muted-foreground">
            An API token for {window.location.host}, such as its CHUNK_OPERATOR_TOKEN. Kept for this tab only.
          </p>
        </div>
        <ErrorText error={attempt.error ? errorMessage(attempt.error) : undefined} />
        <Button type="submit" className="w-full" disabled={!token.trim() || attempt.isPending}>
          {attempt.isPending ? "Checking…" : "Continue"}
        </Button>
      </form>
    </div>
  );
}
