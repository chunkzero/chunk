import { Link, Outlet, useParams } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { EmptyState, Page } from "@/components/page";
import { Commit } from "@/components/repo-source";
import { useProject, useStatus } from "@/lib/api";
import { sections, timeAgo } from "@/lib/format";
import { Missing } from "@/routes/project";

const route = "/p/$project/$deployment";

function useDeployment() {
    const params = useParams({ from: route });
    const { project, data } = useProject(params.project);
    const deployment = project?.deployments.find((entry) => entry.id === params.deployment);
    return { params, project, deployment, loaded: data !== undefined };
}

export function Deployment() {
    const { params, deployment, loaded } = useDeployment();
    if (loaded && !deployment) return <Missing />;

    return (
        <>
            <div className="border-b bg-card">
                <nav className="mx-auto flex max-w-6xl gap-1 px-4 text-sm">
                    {sections.map((section) => (
                        <Link
                            key={section.slug}
                            to={`${route}/${section.slug}` as typeof route}
                            params={params}
                            activeOptions={{ exact: section.slug === "" }}
                            className="border-b-2 px-2 py-2.5"
                            activeProps={{ className: "border-foreground" }}
                            inactiveProps={{
                                className:
                                    "border-transparent text-muted-foreground hover:text-foreground",
                            }}
                        >
                            {section.title}
                        </Link>
                    ))}
                </nav>
            </div>
            <Page>
                <Outlet />
            </Page>
        </>
    );
}

const capabilities = [
    { key: "functions", label: "Functions" },
    { key: "reconciliation", label: "Session runtime" },
    { key: "asset_uploads", label: "Asset storage" },
] as const;

export function Health() {
    const status = useStatus();
    const { deployment } = useDeployment();

    return (
        <div className="grid gap-4 md:grid-cols-2">
            <Table title="Deployment">
                <Row label="Build">
                    <Commit
                        commit={deployment?.commit ?? null}
                        gitRef={deployment?.git_ref ?? null}
                    />
                </Row>
                <Row label="Deployed">
                    {deployment?.deployed_at ? timeAgo(deployment.deployed_at) : "Unknown"}
                </Row>
                <Row label="Backend">{window.location.host}</Row>
                <Row label="Version">{status.data ? `v${status.data.version}` : "…"}</Row>
            </Table>
            <Table title="Capabilities">
                {capabilities.map((capability) => (
                    <Row key={capability.key} label={capability.label}>
                        {status.data?.[capability.key] ? "Enabled" : "Not available"}
                    </Row>
                ))}
            </Table>
        </div>
    );
}

function Table({ title, children }: { title: string; children: ReactNode }) {
    return (
        <section className="rounded-lg border bg-card">
            <h2 className="border-b px-5 py-3 text-sm font-medium">{title}</h2>
            <dl className="divide-y text-sm">{children}</dl>
        </section>
    );
}

function Row({ label, children }: { label: string; children: ReactNode }) {
    return (
        <div className="flex items-center justify-between gap-4 px-5 py-3">
            <dt className="text-muted-foreground">{label}</dt>
            <dd className="font-mono text-xs">{children}</dd>
        </div>
    );
}

export function Section({ title }: { title: string }) {
    return <EmptyState>{title} will appear here once the backend supports them.</EmptyState>;
}
