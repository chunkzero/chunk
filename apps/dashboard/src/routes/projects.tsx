import { Link } from "@tanstack/react-router";
import { ChevronRight } from "lucide-react";
import { EmptyState, Page } from "@/components/page";
import { RepoSource } from "@/components/repo-source";
import { Monitoring } from "@/components/monitoring";
import { useProjects } from "@/lib/api";

export function Projects() {
    const projects = useProjects();

    return (
        <Page>
            <h1 className="text-xl font-semibold tracking-tight">Overview</h1>
            <Monitoring />
            <h2 className="text-base font-semibold">Applications</h2>
            {projects.data?.length === 0 ? (
                <EmptyState>No applications yet.</EmptyState>
            ) : (
                <ul className="divide-y rounded-lg border bg-card">
                    {projects.data?.map((project) => (
                        <li key={project.id} className="space-y-1 px-5 py-4 hover:bg-accent/60">
                            <Link
                                to="/p/$project"
                                params={{ project: project.id }}
                                className="flex items-center gap-4"
                            >
                                <div className="min-w-0 flex-1 space-y-1">
                                    <p className="font-medium">{project.name}</p>
                                </div>
                                <span className="text-sm text-muted-foreground">
                                    {project.deployments.length}{" "}
                                    {project.deployments.length === 1 ? "target" : "targets"}
                                </span>
                                <ChevronRight className="size-4 text-muted-foreground/60" />
                            </Link>
                            <RepoSource source={project.source} />
                        </li>
                    ))}
                </ul>
            )}
            {projects.error && (
                <p role="alert" className="text-sm text-destructive">
                    {projects.error.message}
                </p>
            )}
        </Page>
    );
}
