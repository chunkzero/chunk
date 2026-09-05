import { Link, useParams } from "@tanstack/react-router";
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
import { HugeiconsIcon } from "@hugeicons/react";
import { ArrowRight01Icon, Logout01Icon } from "@hugeicons/core-free-icons";

export function TopNav() {
    const params = useParams({ strict: false });
    const projects = useProjects();
    const status = useStatus();
    const { disconnect } = useSession();
    const project = projects.data?.find((entry) => entry.id === params.project);
    const deployment = project?.deployments.find((entry) => entry.id === params.deployment);

    return (
        <header className="border-b bg-card">
            <div className="mx-auto flex h-13 max-w-6xl items-center justify-between gap-4 px-6">
                <nav
                    aria-label="Breadcrumb"
                    className="flex min-w-0 items-center gap-2 overflow-x-auto whitespace-nowrap text-sm [&>*]:shrink-0"
                >
                    <Link to="/" className="flex items-center gap-2 font-semibold">
                        <span className="size-4 rounded-[3px] bg-foreground" />
                        chunk
                    </Link>
                    <span className="hidden text-muted-foreground sm:inline">
                        {window.location.hostname}
                    </span>
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
                <div className="flex shrink-0 items-center gap-3">
                    <Link
                        to="/server"
                        className="text-sm text-muted-foreground hover:text-foreground"
                    >
                        Logs
                    </Link>
                    <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                            <Button
                                variant="ghost"
                                size="sm"
                                className="gap-2 font-mono text-xs text-muted-foreground"
                            >
                                <StatusDot ok={status.isSuccess} />
                                {status.data ? `v${status.data.version}` : "…"}
                            </Button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent align="end">
                            <DropdownMenuItem onSelect={disconnect}>
                                <HugeiconsIcon icon={Logout01Icon} />
                                Disconnect
                            </DropdownMenuItem>
                        </DropdownMenuContent>
                    </DropdownMenu>
                </div>
            </div>
            {project && deployment && <Tabs project={project.id} deployment={deployment.id} />}
        </header>
    );
}

function Tabs({ project, deployment }: { project: string; deployment: string }) {
    return (
        <LayoutGroup id="deployment-tabs">
            <nav className="mx-auto flex max-w-6xl gap-1 overflow-x-auto px-4 text-sm">
                {sections.map((section) => (
                    <Link
                        key={section.to}
                        to={section.to}
                        params={{ project, deployment }}
                        activeOptions={{ exact: true }}
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

const Crumb = () => (
    <HugeiconsIcon icon={ArrowRight01Icon} className="size-4 text-muted-foreground/60" />
);

export function StatusDot({ ok }: { ok: boolean }) {
    return (
        <span
            className={`inline-block size-2 rounded-full ${ok ? "bg-success" : "bg-destructive"}`}
        />
    );
}
