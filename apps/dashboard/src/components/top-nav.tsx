import { Link, useParams } from "@tanstack/react-router";
import { ChevronRight, LogOut } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useProjects, useStatus } from "@/lib/api";
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
        </header>
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
