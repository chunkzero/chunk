import { Link, useParams } from "@tanstack/react-router";
import { ChevronRight, LogOut } from "lucide-react";
import { LayoutGroup, motion } from "motion/react";
import { Button } from "@/components/ui/button";
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useProjects, useStatus } from "@/lib/api";
import { sections } from "@/lib/format";
import { useSession } from "@/lib/session";

export function TopNav() {
    const params = useParams({ strict: false });
    const projects = useProjects();
    const status = useStatus();
    const { disconnect } = useSession();
    const project = projects.data?.find((entry) => entry.id === params.project);
    const deployment = project?.deployments.find((entry) => entry.id === params.deployment);

    return (
        <header className="border-b bg-card">
            <div className="mx-auto flex h-13 max-w-6xl items-center justify-between px-6">
                <nav className="flex items-center gap-2 text-sm">
                    <Link to="/" className="flex items-center gap-2 font-semibold">
                        <span className="size-4 rounded-[3px] bg-foreground" />
                        chunk
                    </Link>
                    <span className="text-muted-foreground">{window.location.hostname}</span>
                    {project && (
                        <>
                            <Crumb />
                            <Link
                                to="/p/$project"
                                params={{ project: project.id }}
                                className="font-medium"
                            >
                                {project.name}
                            </Link>
                        </>
                    )}
                    {project && deployment && (
                        <>
                            <Crumb />
                            <Link
                                to="/p/$project/$deployment"
                                params={{ project: project.id, deployment: deployment.id }}
                                className="font-medium"
                            >
                                {deployment.name}
                            </Link>
                        </>
                    )}
                </nav>
                <DropdownMenu>
                    <DropdownMenuTrigger asChild>
                        <Button
                            variant="ghost"
                            size="sm"
                            className="gap-2 font-mono text-xs text-muted-foreground"
                        >
                            <StatusDot ok={!status.isError} />
                            {status.data ? `v${status.data.version}` : "…"}
                        </Button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end">
                        <DropdownMenuItem onSelect={disconnect}>
                            <LogOut />
                            Disconnect
                        </DropdownMenuItem>
                    </DropdownMenuContent>
                </DropdownMenu>
            </div>
            {project && deployment && <Tabs project={project.id} deployment={deployment.id} />}
        </header>
    );
}

function Tabs({ project, deployment }: { project: string; deployment: string }) {
    return (
        <LayoutGroup id="deployment-tabs">
            <nav className="mx-auto flex max-w-6xl gap-1 px-4 text-sm">
                {sections.map((section) => (
                    <Link
                        key={section.slug}
                        to={`/p/$project/$deployment/${section.slug}` as "/p/$project/$deployment"}
                        params={{ project, deployment }}
                        activeOptions={{ exact: section.slug === "" }}
                        className="relative px-2 py-2.5"
                        inactiveProps={{ className: "text-muted-foreground hover:text-foreground" }}
                    >
                        {({ isActive }) => (
                            <>
                                {section.title}
                                {isActive && (
                                    <motion.span
                                        layoutId="tab-underline"
                                        className="absolute inset-x-0 -bottom-px h-0.5 bg-foreground"
                                        transition={{ type: "spring", stiffness: 500, damping: 40 }}
                                    />
                                )}
                            </>
                        )}
                    </Link>
                ))}
            </nav>
        </LayoutGroup>
    );
}

const Crumb = () => <ChevronRight className="size-4 text-muted-foreground/60" />;

export function StatusDot({ ok }: { ok: boolean }) {
    return (
        <span
            className={`inline-block size-2 rounded-full ${ok ? "bg-success" : "bg-destructive"}`}
        />
    );
}
