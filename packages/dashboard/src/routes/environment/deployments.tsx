import { useParams } from "@tanstack/react-router";
import { useState } from "react";

import { ConfirmDialog } from "../../components/confirm-dialog.tsx";
import { ErrorText } from "../../components/page.tsx";
import { Panel, PanelNote } from "../../components/panel.tsx";
import { ReleaseId } from "../../components/release.tsx";
import { Status } from "../../components/status.tsx";
import { Button } from "../../components/ui/button.tsx";
import { fieldClass } from "../../components/ui/input.tsx";
import { Label } from "../../components/ui/label.tsx";
import { DeploymentState } from "../../gen/chunk/management/v1/common_pb.ts";
import type { Deployment } from "../../gen/chunk/management/v1/deployments_pb.ts";
import type { Environment } from "../../gen/chunk/management/v1/projects_pb.ts";
import { api, errorMessage, newRequestId, refresh } from "../../lib/client.ts";
import { deploymentStatus, timeAgo, triggerLabels } from "../../lib/format.ts";
import { recentDeployments, useDeployments, useEnvironment, useEnvironments } from "../../lib/queries.ts";

type Dialog = { kind: "promote" } | { kind: "rollback"; deployment: Deployment } | null;

export function Deployments() {
  const { project, environment: environmentId } = useParams({ from: "/p/$project/$environment" });
  const environment = useEnvironment(environmentId).data;
  const deployments = useDeployments(environmentId);
  const [dialog, setDialog] = useState<Dialog>(null);
  const close = () => setDialog(null);

  return (
    <Panel
      title="Deployments"
      action={
        <Button
          size="sm"
          variant="outline"
          disabled={!environment?.activeDeploymentId}
          title={environment?.activeDeploymentId ? undefined : "Nothing is deployed here yet"}
          onClick={() => setDialog({ kind: "promote" })}
        >
          Promote
        </Button>
      }
    >
      <ErrorText error={deployments.error ? errorMessage(deployments.error) : undefined} className="px-5 py-4" />
      {deployments.data?.length === 0 ? (
        <PanelNote>Nothing deployed yet. Deploy a release with the chunk CLI.</PanelNote>
      ) : (
        <ul className="divide-y">
          {deployments.data?.map((deployment) => (
            <li key={deployment.id} className="flex items-center gap-4 px-5 py-3">
              <div className="min-w-0 flex-1 space-y-1">
                <p className="flex items-center gap-2 text-sm">
                  <ReleaseId id={deployment.releaseId} />
                  <span className="text-xs text-muted-foreground">{triggerLabels[deployment.trigger]}</span>
                </p>
                {deployment.message && <p className="text-xs break-words text-destructive">{deployment.message}</p>}
              </div>
              <time className="shrink-0 text-xs text-muted-foreground">{timeAgo(deployment.createTime)}</time>
              <div className="w-24 shrink-0">
                <Status status={deploymentStatus[deployment.state]} />
              </div>
              <div className="w-24 shrink-0 text-right">
                {deployment.state === DeploymentState.SUPERSEDED && (
                  <Button size="xs" variant="ghost" onClick={() => setDialog({ kind: "rollback", deployment })}>
                    Roll back
                  </Button>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}
      {deployments.data?.length === recentDeployments && (
        <p className="border-t px-5 py-3 text-xs text-muted-foreground">
          Showing the {recentDeployments} most recent deployments.
        </p>
      )}
      {environment && dialog?.kind === "promote" && (
        <PromoteDialog project={project} source={environment} onClose={close} />
      )}
      {environment && dialog?.kind === "rollback" && (
        <RollbackDialog environment={environment} deployment={dialog.deployment} onClose={close} />
      )}
    </Panel>
  );
}

function PromoteDialog({ project, source, onClose }: { project: string; source: Environment; onClose: () => void }) {
  const [requestId] = useState(newRequestId);
  const targets = useEnvironments(project).data?.filter((environment) => environment.id !== source.id) ?? [];
  const [target, setTarget] = useState("");
  const targetId = target || targets[0]?.id || "";

  return (
    <ConfirmDialog
      title="Promote"
      description={`Deploys the release active in ${source.name} to another environment. Data and secrets stay where they are.`}
      confirmLabel="Promote"
      disabled={!targetId}
      action={async () => {
        await api.deployments.promote({ requestId, sourceEnvironmentId: source.id, targetEnvironmentId: targetId });
        await refresh("deployments", "environment", "environments");
      }}
      onClose={onClose}
    >
      {targets.length === 0 ? (
        <p className="text-sm text-muted-foreground">This project has no other environment.</p>
      ) : (
        <div className="space-y-2">
          <Label htmlFor="target">Target environment</Label>
          <select
            id="target"
            className={`h-9 ${fieldClass}`}
            value={targetId}
            onChange={(event) => setTarget(event.target.value)}
          >
            {targets.map((environment) => (
              <option key={environment.id} value={environment.id}>
                {environment.name}
              </option>
            ))}
          </select>
        </div>
      )}
    </ConfirmDialog>
  );
}

function RollbackDialog({
  environment,
  deployment,
  onClose,
}: {
  environment: Environment;
  deployment: Deployment;
  onClose: () => void;
}) {
  const [requestId] = useState(newRequestId);
  return (
    <ConfirmDialog
      title="Roll back"
      description={
        <>
          Deploys release <span className="font-mono break-all">{deployment.releaseId}</span> to {environment.name}{" "}
          again.
        </>
      }
      confirmLabel="Roll back"
      action={async () => {
        await api.deployments.rollback({ requestId, environmentId: environment.id, deploymentId: deployment.id });
        await refresh("deployments", "environment", "environments");
      }}
      onClose={onClose}
    />
  );
}
