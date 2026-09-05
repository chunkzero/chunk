import { Link, useParams } from "@tanstack/react-router";
import { ChevronRight, GitBranch, GitCommitHorizontal } from "lucide-react";
import { EmptyState, Page } from "@/components/page";
import { RepoSource } from "@/components/repo-source";
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
                                className="flex items-center gap-4 px-5 py-4 hover:bg-accent/60"
                            >
                                <div className="min-w-0 flex-1 space-y-1">
                                    <p className="font-medium">{deployment.name}</p>
                                    <p className="flex flex-wrap items-center gap-x-2 text-xs text-muted-foreground">
                                        <span>{labels[deployment.environment]}</span>
                                        {deployment.git_ref && (
                                            <>
                                                <Dot />
                                                <span className="flex items-center gap-1 font-mono">
                                                    <GitBranch className="size-3" />
                                                    {deployment.git_ref}
                                                </span>
                                            </>
                                        )}
                                        {deployment.commit && (
                                            <>
                                                <Dot />
                                                <span className="flex items-center gap-1 font-mono">
                                                    <GitCommitHorizontal className="size-3" />
                                                    {deployment.commit.slice(0, 7)}
                                                </span>
                                            </>
                                        )}
                                    </p>
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

const Dot = () => <span aria-hidden>·</span>;

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
