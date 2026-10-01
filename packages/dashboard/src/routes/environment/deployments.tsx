import * as stylex from "@stylexjs/stylex";
import { useParams } from "@tanstack/react-router";
import { useState } from "react";

import { ConfirmDialog } from "../../components/confirm-dialog.tsx";
import { DeployGuideLink } from "../../components/deploy-guide.tsx";
import { ErrorText } from "../../components/page.tsx";
import { listStyles, Panel, PanelNote } from "../../components/panel.tsx";
import { ReleaseId } from "../../components/release.tsx";
import { Status } from "../../components/status.tsx";
import { Button } from "../../components/ui/button.tsx";
import { fieldStyles } from "../../components/ui/input.tsx";
import { Label } from "../../components/ui/label.tsx";
import { DeploymentState } from "../../gen/chunk/management/v1/common_pb.ts";
import type { Deployment } from "../../gen/chunk/management/v1/deployments_pb.ts";
import type { Environment } from "../../gen/chunk/management/v1/projects_pb.ts";
import { api, errorMessage, refresh, useRequestId } from "../../lib/client.ts";
import { deploymentStatus, timeAgo, triggerLabels } from "../../lib/format.ts";
import { recentDeployments, useDeployments, useEnvironment, useEnvironments } from "../../lib/queries.ts";
import { colors, fonts, fontSizes, lineHeights, space } from "../../tokens.stylex.ts";

const styles = stylex.create({
  error: { paddingInline: space.s5, paddingBlock: space.s4 },
  row: { display: "flex", alignItems: "center", gap: space.s4, paddingInline: space.s5, paddingBlock: space.s3 },
  summary: { display: "flex", flexDirection: "column", gap: space.s1, flex: 1, minWidth: 0 },
  release: { display: "flex", alignItems: "center", gap: space.s2, fontSize: fontSizes.sm, lineHeight: lineHeights.sm },
  small: { fontSize: fontSizes.xs, lineHeight: lineHeights.xs, color: colors.mutedForeground },
  message: {
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
    overflowWrap: "break-word",
    color: colors.destructive,
  },
  time: { flexShrink: 0 },
  column: { flexShrink: 0, width: "6.6rem" },
  action: { textAlign: "right" },
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
  note: { fontSize: fontSizes.sm, lineHeight: lineHeights.sm, color: colors.mutedForeground },
  field: { display: "flex", flexDirection: "column", gap: space.s2 },
  select: { height: "2.475rem" },
  id: { fontFamily: fonts.mono, wordBreak: "break-all" },
});

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
      <ErrorText error={deployments.error ? errorMessage(deployments.error) : undefined} style={styles.error} />
      {deployments.data?.length === 0 ? (
        <PanelNote>
          Nothing deployed yet. Upload and deploy a release through the management API, as the <DeployGuideLink />{" "}
          shows.
        </PanelNote>
      ) : (
        <ul>
          {deployments.data?.map((deployment) => (
            <li key={deployment.id} {...stylex.props(listStyles.row, styles.row)}>
              <div {...stylex.props(styles.summary)}>
                <p {...stylex.props(styles.release)}>
                  <ReleaseId id={deployment.releaseId} />
                  <span {...stylex.props(styles.small)}>{triggerLabels[deployment.trigger]}</span>
                </p>
                {deployment.message && <p {...stylex.props(styles.message)}>{deployment.message}</p>}
              </div>
              <time {...stylex.props(styles.small, styles.time)}>{timeAgo(deployment.createTime)}</time>
              <div {...stylex.props(styles.column)}>
                <Status status={deploymentStatus[deployment.state]} />
              </div>
              <div {...stylex.props(styles.column, styles.action)}>
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
        <p {...stylex.props(styles.footnote)}>Showing the {recentDeployments} most recent deployments.</p>
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
  const targets = useEnvironments(project).data?.filter((environment) => environment.id !== source.id) ?? [];
  const [target, setTarget] = useState("");
  const targetId = target || targets[0]?.id || "";
  const requestId = useRequestId(source.id, targetId);

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
        <p {...stylex.props(styles.note)}>This project has no other environment.</p>
      ) : (
        <div {...stylex.props(styles.field)}>
          <Label htmlFor="target">Target environment</Label>
          <select
            id="target"
            {...stylex.props(fieldStyles.field, styles.select)}
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
  const requestId = useRequestId(environment.id, deployment.id);
  return (
    <ConfirmDialog
      title="Roll back"
      description={
        <>
          Deploys release <span {...stylex.props(styles.id)}>{deployment.releaseId}</span> to {environment.name} again.
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
