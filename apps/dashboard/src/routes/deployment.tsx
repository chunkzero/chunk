import { Outlet, useParams } from "@tanstack/react-router";
import { Page } from "@/components/page";
import { useProject } from "@/lib/api";
import { Missing } from "@/routes/project";

export function useDeployment() {
    const params = useParams({ from: "/p/$project/$deployment" });
    const { project, data, error } = useProject(params.project);
    const deployment = project?.deployments.find((entry) => entry.id === params.deployment);
    return { params, project, deployment, loaded: data !== undefined, error };
}

export function Deployment() {
    const { deployment, loaded, error } = useDeployment();
    if (error)
        return (
            <Page>
                <p role="alert" className="text-sm text-destructive">
                    Unable to load this deployment. Retrying…
                </p>
            </Page>
        );
    if (!loaded)
        return (
            <Page>
                <p className="text-sm text-muted-foreground">Loading deployment…</p>
            </Page>
        );
    if (loaded && !deployment) return <Missing />;
    return (
        <Page>
            <Outlet />
        </Page>
    );
}
