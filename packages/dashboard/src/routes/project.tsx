import { useParams } from "@tanstack/react-router";

import { ListLink } from "../components/list-link.tsx";
import { ErrorText, Page } from "../components/page.tsx";
import { Panel, PanelNote } from "../components/panel.tsx";
import { Release } from "../components/release.tsx";
import { Status } from "../components/status.tsx";
import { errorMessage } from "../lib/client.ts";
import { environmentStatus } from "../lib/format.ts";
import { useEnvironments, useProject } from "../lib/queries.ts";

export function Project() {
  const { project: projectId } = useParams({ from: "/p/$project" });
  const project = useProject(projectId);
  const environments = useEnvironments(projectId);
  const error = project.error ?? environments.error;

  return (
    <Page>
      <h1 className="text-xl font-semibold tracking-tight">{project.data?.name ?? "…"}</h1>
      <ErrorText error={error ? errorMessage(error) : undefined} />
      <Panel title="Environments">
        {environments.data?.length === 0 ? (
          <PanelNote>No environments yet.</PanelNote>
        ) : (
          <ul className="divide-y">
            {environments.data?.map((environment) => (
              <li key={environment.id}>
                <ListLink to="/p/$project/$environment" params={{ project: projectId, environment: environment.id }}>
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-sm font-medium">{environment.name}</p>
                    <p className="truncate font-mono text-xs text-muted-foreground">
                      {environment.joinAddress || "No hostname"}
                    </p>
                  </div>
                  <div className="hidden text-right text-xs sm:block">
                    <Release deploymentId={environment.activeDeploymentId} />
                  </div>
                  <div className="w-28 text-right">
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
