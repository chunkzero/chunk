import * as stylex from "@stylexjs/stylex";
import { useParams } from "@tanstack/react-router";

import { ListLink } from "../components/list-link.tsx";
import { ErrorText, Page } from "../components/page.tsx";
import { listStyles, Panel, PanelNote } from "../components/panel.tsx";
import { Release, ShortId } from "../components/release.tsx";
import { Status } from "../components/status.tsx";
import { errorMessage } from "../lib/client.ts";
import { environmentStatus, formatBytes, timeAgo } from "../lib/format.ts";
import { useAssetRevisions, useEnvironments, useProject } from "../lib/queries.ts";
import { colors, fonts, fontSizes, lineHeights, space } from "../tokens.stylex.ts";

const styles = stylex.create({
  title: { fontSize: fontSizes.xl, lineHeight: lineHeights.xl, fontWeight: 600, letterSpacing: "-0.025em" },
  names: { flex: 1, minWidth: 0 },
  name: {
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
    fontWeight: 500,
  },
  address: {
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
    fontFamily: fonts.mono,
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
    color: colors.mutedForeground,
  },
  release: {
    display: { default: "none", "@media (min-width: 40rem)": "block" },
    textAlign: "right",
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
  },
  status: { width: "7.7rem", textAlign: "right" },
  revision: {
    display: "flex",
    alignItems: "center",
    gap: space.s4,
    paddingInline: space.s5,
    paddingBlock: space.s3,
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
  },
  revisionId: { flex: 1, minWidth: 0 },
  muted: { flexShrink: 0, color: colors.mutedForeground },
  head: { flexShrink: 0, fontWeight: 500 },
});

export function Project() {
  const { project: projectId } = useParams({ from: "/p/$project" });
  const project = useProject(projectId);
  const environments = useEnvironments(projectId);
  const assets = useAssetRevisions(projectId);
  const error = project.error ?? environments.error ?? assets.error;

  return (
    <Page>
      <h1 {...stylex.props(styles.title)}>{project.data?.name ?? "…"}</h1>
      <ErrorText error={error ? errorMessage(error) : undefined} />
      <Panel title="Environments">
        {environments.data?.length === 0 ? (
          <PanelNote>No environments yet.</PanelNote>
        ) : (
          <ul>
            {environments.data?.map((environment) => (
              <li key={environment.id} {...stylex.props(listStyles.row)}>
                <ListLink to="/p/$project/$environment" params={{ project: projectId, environment: environment.id }}>
                  <div {...stylex.props(styles.names)}>
                    <p {...stylex.props(styles.name)}>{environment.name}</p>
                    <p {...stylex.props(styles.address)}>{environment.joinAddress || "No hostname"}</p>
                  </div>
                  <div {...stylex.props(styles.release)}>
                    <Release deploymentId={environment.activeDeploymentId} />
                  </div>
                  <div {...stylex.props(styles.status)}>
                    <Status status={environmentStatus[environment.state]} />
                  </div>
                </ListLink>
              </li>
            ))}
          </ul>
        )}
      </Panel>
      <Panel title="Asset revisions">
        {assets.data?.revisions.length === 0 ? (
          <PanelNote>No assets yet. Run chunk assets push to publish them.</PanelNote>
        ) : (
          <ul>
            {assets.data?.revisions.map((revision) => (
              <li key={revision.id} {...stylex.props(listStyles.row, styles.revision)}>
                <span {...stylex.props(styles.revisionId)}>
                  <ShortId id={revision.id} />
                </span>
                {revision.id === assets.data.headId && <span {...stylex.props(styles.head)}>Head</span>}
                <span {...stylex.props(styles.muted)}>{formatBytes(revision.sizeBytes)}</span>
                <time {...stylex.props(styles.muted)}>{timeAgo(revision.createTime)}</time>
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </Page>
  );
}
