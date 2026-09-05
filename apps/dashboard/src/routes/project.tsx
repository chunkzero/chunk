import { Link, useParams } from "@tanstack/react-router";
import { ChevronRight } from "lucide-react";
import { EmptyState, Page } from "@/components/page";
import { Commit, RepoSource } from "@/components/repo-source";
import { type Deployment, useProject } from "@/lib/api";
import { timeAgo } from "@/lib/format";

const order: Deployment["environment"][] = ["production", "development", "preview"];
const labels = { production: "Production", development: "Development", preview: "Preview" };

export function Project() {
    const { project: projectId } = useParams({ from: "/p/$project" });
    const { project, data } = useProject(projectId);

    if (data && !project) return <Missing />;
    if (!project) return null;

    const deployments = [...project.deployments].sort(
        (a, b) => order.indexOf(a.environment) - order.indexOf(b.environment),
    );

    return (
        <Page>
            <div className="space-y-1">
                <h1 className="text-xl font-semibold tracking-tight">{project.name}</h1>
                <RepoSource source={project.source} />
            </div>
            {deployments.length === 0 ? (
                <EmptyState>No deployments yet.</EmptyState>
            ) : (
                <ul className="divide-y rounded-lg border bg-card">
                    {deployments.map((deployment) => (
                        <li key={deployment.id}>
                            <Link
                                to="/p/$project/$deployment"
                                params={{ project: project.id, deployment: deployment.id }}
                                className="grid grid-cols-[1fr_auto_auto] items-center gap-x-6 gap-y-1 px-5 py-4 hover:bg-accent/60 sm:grid-cols-[minmax(0,1.2fr)_minmax(0,1.5fr)_auto_auto]"
                            >
                                <div className="min-w-0">
                                    <p className="font-medium">{deployment.name}</p>
                                    <p className="text-sm text-muted-foreground">
                                        {labels[deployment.environment]}
                                    </p>
                                </div>
                                <div className="col-span-3 sm:col-span-1">
                                    <Commit
                                        commit={deployment.commit}
                                        gitRef={deployment.git_ref}
                                    />
                                </div>
                                <span className="text-sm text-muted-foreground">
                                    {deployment.deployed_at ? timeAgo(deployment.deployed_at) : ""}
                                </span>
                                <ChevronRight className="size-4 text-muted-foreground/60" />
                            </Link>
                        </li>
                    ))}
                </ul>
            )}
        </Page>
    );
}

export function Missing() {
    return (
        <Page>
            <p className="text-sm text-muted-foreground">
                Nothing here.{" "}
                <Link to="/" className="text-link underline-offset-4 hover:underline">
                    Back to applications
                </Link>
            </p>
        </Page>
    );
}
