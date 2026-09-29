import { DeployGuideLink } from "../components/deploy-guide.tsx";
import { ListLink } from "../components/list-link.tsx";
import { EmptyState, ErrorText, Page } from "../components/page.tsx";
import { errorMessage } from "../lib/client.ts";
import { timeAgo } from "../lib/format.ts";
import { useProjects } from "../lib/queries.ts";

export function Projects() {
  const projects = useProjects();

  return (
    <Page>
      <h1 className="text-xl font-semibold tracking-tight">Projects</h1>
      {projects.data?.length === 0 ? (
        <EmptyState>
          <p>
            No projects yet. Create one through the management API, as the <DeployGuideLink /> shows.
          </p>
        </EmptyState>
      ) : (
        <ul className="divide-y overflow-hidden rounded-lg border bg-card">
          {projects.data?.map((project) => (
            <li key={project.id}>
              <ListLink to="/p/$project" params={{ project: project.id }}>
                <span className="min-w-0 flex-1 truncate font-medium">{project.name}</span>
                <span className="text-xs text-muted-foreground">Created {timeAgo(project.createTime)}</span>
              </ListLink>
            </li>
          ))}
        </ul>
      )}
      <ErrorText error={projects.error ? errorMessage(projects.error) : undefined} />
    </Page>
  );
}
