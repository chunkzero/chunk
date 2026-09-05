import { Link } from "@tanstack/react-router";
import { EmptyState, Page } from "@/components/page";
import { RepoSource } from "@/components/repo-source";
import { Monitoring } from "@/components/monitoring";
import { useProjects } from "@/lib/api";
import { HugeiconsIcon } from "@hugeicons/react";
import { ArrowRight01Icon } from "@hugeicons/core-free-icons";

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
                <ul className="divide-y overflow-hidden rounded-lg border bg-card">
                    {projects.data?.map((project) => (
                        <li
                            key={project.id}
                            className="relative space-y-1 px-5 py-4 transition-colors first:rounded-t-lg last:rounded-b-lg hover:bg-accent/60 focus-within:bg-accent/60"
                        >
                            <Link
                                to="/p/$project"
                                params={{ project: project.id }}
                                className="flex items-center gap-4 after:absolute after:inset-0 after:rounded-[inherit] after:content-[''] focus-visible:outline-none focus-visible:after:ring-2 focus-visible:after:ring-inset focus-visible:after:ring-ring"
                            >
                                <div className="min-w-0 flex-1 space-y-1">
                                    <p className="font-medium">{project.name}</p>
                                </div>
                                <span className="text-sm text-muted-foreground">
                                    {project.deployments.length}{" "}
                                    {project.deployments.length === 1 ? "target" : "targets"}
                                </span>
                                <HugeiconsIcon
                                    icon={ArrowRight01Icon}
                                    className="size-4 text-muted-foreground/60"
                                />
                            </Link>
                            <div className="[&_a[href]]:relative [&_a[href]]:z-10">
                                <RepoSource source={project.source} />
                            </div>
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
