import { Add01Icon, Delete02Icon, EditIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useParams } from "@tanstack/react-router";
import { useState } from "react";

import { ConfirmDialog } from "../../components/confirm-dialog.tsx";
import { ErrorText } from "../../components/page.tsx";
import { Panel, PanelNote } from "../../components/panel.tsx";
import { Button } from "../../components/ui/button.tsx";
import { fieldClass, Input } from "../../components/ui/input.tsx";
import { Label } from "../../components/ui/label.tsx";
import { api, errorMessage, refresh, useRequestId } from "../../lib/client.ts";
import { timeAgo } from "../../lib/format.ts";
import { useSecrets } from "../../lib/queries.ts";

type Dialog = { kind: "set"; name?: string } | { kind: "delete"; name: string } | null;

export function Secrets() {
  const { environment } = useParams({ from: "/p/$project/$environment" });
  const secrets = useSecrets(environment);
  const [dialog, setDialog] = useState<Dialog>(null);
  const close = () => setDialog(null);

  return (
    <Panel
      title="Secrets"
      action={
        <Button size="sm" variant="outline" onClick={() => setDialog({ kind: "set" })}>
          <HugeiconsIcon icon={Add01Icon} /> Add secret
        </Button>
      }
    >
      <ErrorText error={secrets.error ? errorMessage(secrets.error) : undefined} className="px-5 py-4" />
      {secrets.data?.length === 0 ? (
        <PanelNote>No secrets yet.</PanelNote>
      ) : (
        <ul className="divide-y">
          {secrets.data?.map((secret) => (
            <li key={secret.name} className="flex items-center gap-4 px-5 py-2.5">
              <span className="min-w-0 flex-1 truncate font-mono text-sm">{secret.name}</span>
              <span className="text-xs text-muted-foreground tabular-nums">v{secret.version.toString()}</span>
              <span className="w-28 text-right text-xs text-muted-foreground">{timeAgo(secret.updateTime)}</span>
              <div className="flex gap-1">
                <Button
                  size="icon-sm"
                  variant="ghost"
                  aria-label={`Replace ${secret.name}`}
                  title="Replace"
                  onClick={() => setDialog({ kind: "set", name: secret.name })}
                >
                  <HugeiconsIcon icon={EditIcon} />
                </Button>
                <Button
                  size="icon-sm"
                  variant="ghost"
                  aria-label={`Delete ${secret.name}`}
                  title="Delete"
                  onClick={() => setDialog({ kind: "delete", name: secret.name })}
                >
                  <HugeiconsIcon icon={Delete02Icon} />
                </Button>
              </div>
            </li>
          ))}
        </ul>
      )}
      <p className="border-t px-5 py-3 text-xs text-muted-foreground">
        Values are write-only. Running environments receive changes without a redeploy.
      </p>
      {dialog?.kind === "set" && <SetSecretDialog environment={environment} name={dialog.name} onClose={close} />}
      {dialog?.kind === "delete" && (
        <ConfirmDialog
          title="Delete secret"
          description={
            <>
              Environments stop receiving <span className="font-mono">{dialog.name}</span>.
            </>
          }
          confirmLabel="Delete"
          destructive
          action={async () => {
            await api.secrets.deleteSecret({ environmentId: environment, name: dialog.name });
            await refresh("secrets");
          }}
          onClose={close}
        />
      )}
    </Panel>
  );
}

function SetSecretDialog({
  environment,
  name: existing,
  onClose,
}: {
  environment: string;
  name: string | undefined;
  onClose: () => void;
}) {
  const [name, setName] = useState(existing ?? "");
  const [value, setValue] = useState("");
  const requestId = useRequestId(environment, name, value);

  return (
    <ConfirmDialog
      title={existing ? "Replace secret" : "Add secret"}
      description={existing ? "The new value becomes the next version." : undefined}
      confirmLabel="Save"
      action={async () => {
        await api.secrets.setSecret({
          requestId,
          environmentId: environment,
          name,
          value: new TextEncoder().encode(value),
        });
        await refresh("secrets");
      }}
      onClose={onClose}
    >
      <div className="space-y-2">
        <Label htmlFor="secret-name">Name</Label>
        <Input
          id="secret-name"
          required
          pattern="[A-Za-z_][A-Za-z0-9_]{0,127}"
          title="Letters, digits and underscores, not starting with a digit"
          autoComplete="off"
          className="font-mono"
          readOnly={existing !== undefined}
          autoFocus={existing === undefined}
          value={name}
          onChange={(event) => setName(event.target.value)}
        />
      </div>
      <div className="space-y-2">
        <Label htmlFor="secret-value">Value</Label>
        <textarea
          id="secret-value"
          rows={4}
          autoComplete="off"
          spellCheck={false}
          autoFocus={existing !== undefined}
          className={`py-2 font-mono ${fieldClass}`}
          value={value}
          onChange={(event) => setValue(event.target.value)}
        />
      </div>
    </ConfirmDialog>
  );
}
