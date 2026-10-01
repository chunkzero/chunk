import * as stylex from "@stylexjs/stylex";

import { DeployGuideLink } from "../components/deploy-guide.tsx";
import { ListLink } from "../components/list-link.tsx";
import { EmptyState, ErrorText, Page } from "../components/page.tsx";
import { listStyles } from "../components/panel.tsx";
import { errorMessage } from "../lib/client.ts";
import { timeAgo } from "../lib/format.ts";
import { useProjects } from "../lib/queries.ts";
import { colors, fontSizes, lineHeights, radii } from "../tokens.stylex.ts";

const styles = stylex.create({
  title: { fontSize: fontSizes.xl, lineHeight: lineHeights.xl, fontWeight: 600, letterSpacing: "-0.025em" },
  list: {
    overflow: "hidden",
    borderWidth: "1px",
    borderStyle: "solid",
    borderColor: colors.border,
    borderRadius: radii.lg,
    backgroundColor: colors.card,
  },
  name: { flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", fontWeight: 500 },
  created: { fontSize: fontSizes.xs, lineHeight: lineHeights.xs, color: colors.mutedForeground },
});

export function Projects() {
  const projects = useProjects();

  return (
    <Page>
      <h1 {...stylex.props(styles.title)}>Projects</h1>
      {projects.data?.length === 0 ? (
        <EmptyState>
          <p>
            No projects yet. Create one through the management API, as the <DeployGuideLink /> shows.
          </p>
        </EmptyState>
      ) : (
        <ul {...stylex.props(styles.list)}>
          {projects.data?.map((project) => (
            <li key={project.id} {...stylex.props(listStyles.row)}>
              <ListLink to="/p/$project" params={{ project: project.id }}>
                <span {...stylex.props(styles.name)}>{project.name}</span>
                <span {...stylex.props(styles.created)}>Created {timeAgo(project.createTime)}</span>
              </ListLink>
            </li>
          ))}
        </ul>
      )}
      <ErrorText error={projects.error ? errorMessage(projects.error) : undefined} />
    </Page>
  );
}
