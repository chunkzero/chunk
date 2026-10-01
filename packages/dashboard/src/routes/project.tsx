import * as stylex from "@stylexjs/stylex";
import { useParams } from "@tanstack/react-router";

import { ListLink } from "../components/list-link.tsx";
import { ErrorText, Page } from "../components/page.tsx";
import { listStyles, Panel, PanelNote } from "../components/panel.tsx";
import { Release } from "../components/release.tsx";
import { Status } from "../components/status.tsx";
import { errorMessage } from "../lib/client.ts";
import { environmentStatus } from "../lib/format.ts";
import { useEnvironments, useProject } from "../lib/queries.ts";
import { colors, fonts, fontSizes, lineHeights } from "../tokens.stylex.ts";

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
});

export function Project() {
  const { project: projectId } = useParams({ from: "/p/$project" });
  const project = useProject(projectId);
  const environments = useEnvironments(projectId);
  const error = project.error ?? environments.error;

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
    </Page>
  );
}
