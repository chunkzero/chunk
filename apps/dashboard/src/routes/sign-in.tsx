import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { getStatus } from "@/lib/api";
import { useSession } from "@/lib/session";

export function SignIn() {
    const [token, setToken] = useState("");
    const { connect } = useSession();
    const attempt = useMutation({
        mutationFn: (candidate: string) => getStatus(candidate, new AbortController().signal),
        onSuccess: (_, candidate) => connect(candidate),
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
                <div className="flex items-center gap-2 font-semibold">
                    <span className="size-4 rounded-[3px] bg-foreground" />
                    chunk
                </div>
                <div className="space-y-2">
                    <Label htmlFor="token">Management token</Label>
                    <Input
                        id="token"
                        type="password"
                        required
                        autoComplete="off"
                        autoFocus
                        className="font-mono"
                        value={token}
                        onChange={(event) => setToken(event.target.value)}
                        aria-invalid={attempt.isError}
                    />
                    <p className="text-xs text-muted-foreground">
                        The value of CHUNK_MANAGEMENT_TOKEN on {window.location.host}. Kept in
                        memory for this tab.
                    </p>
                </div>
                {attempt.error && (
                    <p role="alert" className="text-sm text-destructive">
                        {attempt.error.message}
                    </p>
                )}
                <Button
                    type="submit"
                    className="w-full"
                    disabled={!token.trim() || attempt.isPending}
                >
                    {attempt.isPending ? "Connecting…" : "Continue"}
                </Button>
            </form>
        </div>
    );
}
