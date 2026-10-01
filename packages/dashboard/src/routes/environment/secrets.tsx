import { Add01Icon, Delete02Icon, EditIcon } from "@hugeicons/core-free-icons";
import * as stylex from "@stylexjs/stylex";
import { useParams } from "@tanstack/react-router";
import { useState } from "react";

import { ConfirmDialog } from "../../components/confirm-dialog.tsx";
import { ErrorText } from "../../components/page.tsx";
import { listStyles, Panel, PanelNote } from "../../components/panel.tsx";
import { Button } from "../../components/ui/button.tsx";
import { fieldStyles, Input } from "../../components/ui/input.tsx";
import { Label } from "../../components/ui/label.tsx";
import { api, errorMessage, refresh, useRequestId } from "../../lib/client.ts";
import { timeAgo } from "../../lib/format.ts";
import { useSecrets } from "../../lib/queries.ts";
import { colors, fonts, fontSizes, lineHeights, space } from "../../tokens.stylex.ts";

const styles = stylex.create({
  error: { paddingInline: space.s5, paddingBlock: space.s4 },
  row: { display: "flex", alignItems: "center", gap: space.s4, paddingInline: space.s5, paddingBlock: space.s2_5 },
  name: {
    flex: 1,
    minWidth: 0,
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
    fontFamily: fonts.mono,
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
  },
  small: { fontSize: fontSizes.xs, lineHeight: lineHeights.xs, color: colors.mutedForeground },
  version: { fontVariantNumeric: "tabular-nums" },
  updated: { width: "7.7rem", textAlign: "right" },
  actions: { display: "flex", gap: space.s1 },
  footnote: {
    paddingInline: space.s5,
    paddingBlock: space.s3,
    borderTopWidth: "1px",
    borderTopStyle: "solid",
    borderTopColor: colors.border,
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
    color: colors.mutedForeground,
  },
  mono: { fontFamily: fonts.mono },
  field: { display: "flex", flexDirection: "column", gap: space.s2 },
  value: { paddingBlock: space.s2, fontFamily: fonts.mono },
});

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
        <Button size="sm" variant="outline" icon={Add01Icon} onClick={() => setDialog({ kind: "set" })}>
          Add secret
        </Button>
      }
    >
      <ErrorText error={secrets.error ? errorMessage(secrets.error) : undefined} style={styles.error} />
      {secrets.data?.length === 0 ? (
        <PanelNote>No secrets yet.</PanelNote>
      ) : (
        <ul>
          {secrets.data?.map((secret) => (
            <li key={secret.name} {...stylex.props(listStyles.row, styles.row)}>
              <span {...stylex.props(styles.name)}>{secret.name}</span>
              <span {...stylex.props(styles.small, styles.version)}>v{secret.version.toString()}</span>
              <span {...stylex.props(styles.small, styles.updated)}>{timeAgo(secret.updateTime)}</span>
              <div {...stylex.props(styles.actions)}>
                <Button
                  size="icon-sm"
                  variant="ghost"
                  icon={EditIcon}
                  aria-label={`Replace ${secret.name}`}
                  title="Replace"
                  onClick={() => setDialog({ kind: "set", name: secret.name })}
                />
                <Button
                  size="icon-sm"
                  variant="ghost"
                  icon={Delete02Icon}
                  aria-label={`Delete ${secret.name}`}
                  title="Delete"
                  onClick={() => setDialog({ kind: "delete", name: secret.name })}
                />
              </div>
            </li>
          ))}
        </ul>
      )}
      <p {...stylex.props(styles.footnote)}>
        Values are write-only. Running environments receive changes without a redeploy.
      </p>
      {dialog?.kind === "set" && <SetSecretDialog environment={environment} name={dialog.name} onClose={close} />}
      {dialog?.kind === "delete" && (
        <ConfirmDialog
          title="Delete secret"
          description={
            <>
              Environments stop receiving <span {...stylex.props(styles.mono)}>{dialog.name}</span>.
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
      <div {...stylex.props(styles.field)}>
        <Label htmlFor="secret-name">Name</Label>
        <Input
          id="secret-name"
          required
          pattern="[A-Za-z_][A-Za-z0-9_]{0,127}"
          title="Letters, digits and underscores, not starting with a digit"
          autoComplete="off"
          style={styles.mono}
          readOnly={existing !== undefined}
          autoFocus={existing === undefined}
          value={name}
          onValueChange={setName}
        />
      </div>
      <div {...stylex.props(styles.field)}>
        <Label htmlFor="secret-value">Value</Label>
        <textarea
          id="secret-value"
          rows={4}
          autoComplete="off"
          spellCheck={false}
          autoFocus={existing !== undefined}
          {...stylex.props(fieldStyles.field, styles.value)}
          value={value}
          onChange={(event) => setValue(event.target.value)}
        />
      </div>
    </ConfirmDialog>
  );
}
