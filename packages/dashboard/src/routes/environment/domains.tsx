import { Add01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useMutation } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";
import { useState } from "react";

import { ConfirmDialog } from "../../components/confirm-dialog.tsx";
import { ErrorText } from "../../components/page.tsx";
import { Panel, PanelNote } from "../../components/panel.tsx";
import { Status } from "../../components/status.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Input } from "../../components/ui/input.tsx";
import { Label } from "../../components/ui/label.tsx";
import { type Domain as DomainData, DomainState } from "../../gen/chunk/management/v1/domains_pb.ts";
import { api, errorMessage, refresh } from "../../lib/client.ts";
import { domainStatus } from "../../lib/format.ts";
import { useDomains } from "../../lib/queries.ts";

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
        <Button size="sm" variant="outline" onClick={() => setDialog({ kind: "add" })}>
          <HugeiconsIcon icon={Add01Icon} /> Add domain
        </Button>
      }
    >
      <ErrorText error={domains.error ? errorMessage(domains.error) : undefined} className="px-5 py-4" />
      {domains.data?.length === 0 ? (
        <PanelNote>No custom domains yet.</PanelNote>
      ) : (
        <ul className="divide-y">
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
              Players can no longer join through <span className="font-mono">{dialog.domain.hostname}</span>.
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
    <li className="space-y-3 px-5 py-4">
      <div className="flex flex-wrap items-center gap-3">
        <span className="min-w-0 flex-1 truncate font-mono text-sm">{domain.hostname}</span>
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
        <p className="text-xs text-muted-foreground">
          The TXT record was not found yet. DNS changes can take a while to spread.
        </p>
      )}
      <ErrorText error={verify.error ? errorMessage(verify.error) : undefined} />
      <table className="w-full table-fixed text-left text-xs">
        <thead className="text-muted-foreground">
          <tr>
            <th className="w-16 py-1 font-normal">Type</th>
            <th className="w-2/5 py-1 font-normal">Name</th>
            <th className="py-1 font-normal">Value</th>
          </tr>
        </thead>
        <tbody className="font-mono">
          {domain.dnsRecords.map((record) => (
            <tr key={`${record.type} ${record.name} ${record.value}`} className="border-t align-top">
              <td className="py-1.5">{record.type}</td>
              <td className="py-1.5 pr-4 break-all">{record.name}</td>
              <td className="py-1.5 break-all select-all">{record.value}</td>
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
      <div className="space-y-2">
        <Label htmlFor="hostname">Hostname</Label>
        <Input
          id="hostname"
          required
          autoFocus
          autoComplete="off"
          placeholder="play.example.com"
          className="font-mono"
          value={hostname}
          onChange={(event) => setHostname(event.target.value)}
        />
      </div>
    </ConfirmDialog>
  );
}
