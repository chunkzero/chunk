import { Add01Icon } from "@hugeicons/core-free-icons";
import * as stylex from "@stylexjs/stylex";
import { useMutation } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";
import { useState } from "react";

import { ConfirmDialog } from "../../components/confirm-dialog.tsx";
import { ErrorText } from "../../components/page.tsx";
import { listStyles, Panel, PanelNote } from "../../components/panel.tsx";
import { Status } from "../../components/status.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Input } from "../../components/ui/input.tsx";
import { Label } from "../../components/ui/label.tsx";
import { type Domain as DomainData, DomainState } from "../../gen/chunk/management/v1/domains_pb.ts";
import { api, errorMessage, refresh } from "../../lib/client.ts";
import { domainStatus } from "../../lib/format.ts";
import { useDomains } from "../../lib/queries.ts";
import { colors, fonts, fontSizes, lineHeights, space } from "../../tokens.stylex.ts";

const styles = stylex.create({
  error: { paddingInline: space.s5, paddingBlock: space.s4 },
  mono: { fontFamily: fonts.mono },
  domain: { display: "flex", flexDirection: "column", gap: space.s3, paddingInline: space.s5, paddingBlock: space.s4 },
  heading: { display: "flex", flexWrap: "wrap", alignItems: "center", gap: space.s3 },
  hostname: {
    flex: 1,
    minWidth: 0,
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
    fontFamily: fonts.mono,
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
  },
  hint: { fontSize: fontSizes.xs, lineHeight: lineHeights.xs, color: colors.mutedForeground },
  records: {
    width: "100%",
    tableLayout: "fixed",
    textAlign: "left",
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
  },
  header: { color: colors.mutedForeground },
  th: { paddingBlock: space.s1, fontWeight: 400 },
  type: { width: "4.4rem" },
  nameColumn: { width: "40%" },
  record: { borderTopWidth: "1px", borderTopStyle: "solid", borderTopColor: colors.border, verticalAlign: "top" },
  td: { paddingBlock: space.s1_5 },
  name: { paddingRight: space.s4, wordBreak: "break-all" },
  value: { wordBreak: "break-all", userSelect: "all" },
  field: { display: "flex", flexDirection: "column", gap: space.s2 },
});

type Dialog = { kind: "add" } | { kind: "remove"; domain: DomainData } | null;

export function Domains() {
  const { environment } = useParams({ from: "/p/$project/$environment" });
  const domains = useDomains(environment);
  const [dialog, setDialog] = useState<Dialog>(null);
  const close = () => setDialog(null);

  return (
    <Panel
      title="Domains"
      action={
        <Button size="sm" variant="outline" icon={Add01Icon} onClick={() => setDialog({ kind: "add" })}>
          Add domain
        </Button>
      }
    >
      <ErrorText error={domains.error ? errorMessage(domains.error) : undefined} style={styles.error} />
      {domains.data?.length === 0 ? (
        <PanelNote>No custom domains yet.</PanelNote>
      ) : (
        <ul>
          {domains.data?.map((domain) => (
            <Domain key={domain.id} domain={domain} onRemove={() => setDialog({ kind: "remove", domain })} />
          ))}
        </ul>
      )}
      {dialog?.kind === "add" && <AddDomainDialog environment={environment} onClose={close} />}
      {dialog?.kind === "remove" && (
        <ConfirmDialog
          title="Remove domain"
          description={
            <>
              Players can no longer join through <span {...stylex.props(styles.mono)}>{dialog.domain.hostname}</span>.
            </>
          }
          confirmLabel="Remove"
          destructive
          action={async () => {
            await api.domains.removeDomain({ domainId: dialog.domain.id });
            await refresh("domains");
          }}
          onClose={close}
        />
      )}
    </Panel>
  );
}

function Domain({ domain, onRemove }: { domain: DomainData; onRemove: () => void }) {
  const verify = useMutation({
    mutationFn: async () => (await api.domains.verifyDomain({ domainId: domain.id })).domain,
    onSuccess: () => refresh("domains"),
  });
  const notFound = verify.data?.state === DomainState.PENDING_VERIFICATION;

  return (
    <li {...stylex.props(listStyles.row, styles.domain)}>
      <div {...stylex.props(styles.heading)}>
        <span {...stylex.props(styles.hostname)}>{domain.hostname}</span>
        <Status status={domainStatus[domain.state]} />
        {domain.state !== DomainState.VERIFIED && (
          <Button size="xs" variant="outline" disabled={verify.isPending} onClick={() => verify.mutate()}>
            {verify.isPending ? "Checking…" : "Verify"}
          </Button>
        )}
        <Button size="xs" variant="ghost" onClick={onRemove}>
          Remove
        </Button>
      </div>
      {notFound && (
        <p {...stylex.props(styles.hint)}>The TXT record was not found yet. DNS changes can take a while to spread.</p>
      )}
      <ErrorText error={verify.error ? errorMessage(verify.error) : undefined} />
      <table {...stylex.props(styles.records)}>
        <thead {...stylex.props(styles.header)}>
          <tr>
            <th {...stylex.props(styles.th, styles.type)}>Type</th>
            <th {...stylex.props(styles.th, styles.nameColumn)}>Name</th>
            <th {...stylex.props(styles.th)}>Value</th>
          </tr>
        </thead>
        <tbody {...stylex.props(styles.mono)}>
          {domain.dnsRecords.map((record) => (
            <tr key={`${record.type} ${record.name} ${record.value}`} {...stylex.props(styles.record)}>
              <td {...stylex.props(styles.td)}>{record.type}</td>
              <td {...stylex.props(styles.td, styles.name)}>{record.name}</td>
              <td {...stylex.props(styles.td, styles.value)}>{record.value}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </li>
  );
}

function AddDomainDialog({ environment, onClose }: { environment: string; onClose: () => void }) {
  const [hostname, setHostname] = useState("");
  return (
    <ConfirmDialog
      title="Add domain"
      description="Players join through this hostname once its DNS records are in place and verified."
      confirmLabel="Add"
      action={async () => {
        await api.domains.addDomain({ environmentId: environment, hostname: hostname.trim() });
        await refresh("domains");
      }}
      onClose={onClose}
    >
      <div {...stylex.props(styles.field)}>
        <Label htmlFor="hostname">Hostname</Label>
        <Input
          id="hostname"
          required
          autoFocus
          autoComplete="off"
          placeholder="play.example.com"
          style={styles.mono}
          value={hostname}
          onValueChange={setHostname}
        />
      </div>
    </ConfirmDialog>
  );
}
