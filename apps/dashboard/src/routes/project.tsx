import { useState } from "react";
import { Link, useParams } from "@tanstack/react-router";

import { Page } from "@/components/page";
import { Panel, Row, Rows } from "@/components/panel";
import { RepoSource } from "@/components/repo-source";
import { SourceDialog } from "@/components/source-dialog";
import { RemoveTargetDialog, TargetDialog } from "@/components/target-dialog";
import { Button } from "@/components/ui/button";
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
    type Deployment,
    type Project as ProjectData,
    useBranches,
    useEditable,
    useProject,
} from "@/lib/api";
import { environmentLabels, environmentOrder, repoLabel, timeAgo } from "@/lib/format";
import { HugeiconsIcon } from "@hugeicons/react";
import {
    ArrowRight01Icon,
    GitBranchIcon,
    MoreHorizontalCircle01Icon,
    EditIcon,
    Add01Icon,
    RepeatIcon,
    Delete02Icon,
    GitCommitIcon,
} from "@hugeicons/core-free-icons";

type Dialog =
    | { kind: "source" }
    | { kind: "add"; branch?: string }
    | { kind: "edit"; target: Deployment }
    | { kind: "remove"; target: Deployment }
    | null;

export function Project() {
    const { project: projectId } = useParams({ from: "/p/$project" });
    const { project, data, error } = useProject(projectId);
    const { editable, reason } = useEditable();
    const [dialog, setDialog] = useState<Dialog>(null);
    const [dialogOpen, setDialogOpen] = useState(false);
    function showDialog(next: Exclude<Dialog, null>) {
        setDialog(next);
        setDialogOpen(true);
    }
    if (error && !project)
        return (
            <Page>
                <p role="alert" className="text-sm text-destructive">
                    Unable to load this application. Retrying…
                </p>
            </Page>
        );
    if (data && !project) return <Missing />;
    if (!project) return <Page>Loading application…</Page>;

    const targets = [...project.deployments].sort(
        (a, b) =>
            environmentOrder.indexOf(a.environment) - environmentOrder.indexOf(b.environment) ||
            a.name.localeCompare(b.name),
    );
    const recent = project.deployments
        .filter((entry) => entry.deployed_at && Number.isFinite(Date.parse(entry.deployed_at)))
        .sort((a, b) => Date.parse(b.deployed_at ?? "") - Date.parse(a.deployed_at ?? ""))
        .slice(0, 5);
    const close = (open: boolean) => {
        setDialogOpen(open);
    };

    return (
        <Page>
            <div className="space-y-1">
                <h1 className="text-xl font-semibold tracking-tight">{project.name}</h1>
                <RepoSource source={project.source} />
            </div>
            {error && (
                <p role="alert" className="text-sm text-destructive">
                    Application data could not refresh. Retrying…
                </p>
            )}
            <div className="grid items-start gap-6 lg:grid-cols-[1.4fr_1fr]">
                <div className="min-w-0 space-y-6">
                    <Panel
                        title="Deployment targets"
                        action={
                            <Button
                                size="sm"
                                variant="outline"
                                disabled={!editable || !project.source}
                                title={
                                    reason ??
                                    (project.source ? undefined : "Link a repository first")
                                }
                                onClick={() => showDialog({ kind: "add" })}
                            >
                                <HugeiconsIcon icon={Add01Icon} /> Add target
                            </Button>
                        }
                    >
                        {targets.length ? (
                            <ul className="divide-y">
                                {targets.map((target) => (
                                    <li
                                        key={target.id}
                                        className="flex items-center pr-3 transition-colors duration-150 last:rounded-b-lg hover:bg-accent/60 focus-within:bg-accent/60"
                                    >
                                        <Link
                                            to="/p/$project/$deployment"
                                            params={{ project: project.id, deployment: target.id }}
                                            className="flex min-w-0 flex-1 items-center gap-3 px-5 py-4"
                                        >
                                            <HugeiconsIcon
                                                icon={GitBranchIcon}
                                                className="size-4 shrink-0 text-muted-foreground"
                                            />
                                            <div className="min-w-0 flex-1">
                                                <p className="truncate text-sm font-medium">
                                                    {target.name}
                                                </p>
                                                <p className="truncate font-mono text-xs text-muted-foreground">
                                                    {target.git_ref ?? "No branch configured"}
                                                </p>
                                            </div>
                                            <div className="shrink-0 text-right text-xs text-muted-foreground">
                                                <p>{environmentLabels[target.environment]}</p>
                                                {!target.deployed_at && (
                                                    <p className="mt-1">Not deployed</p>
                                                )}
                                            </div>
                                            <HugeiconsIcon
                                                icon={ArrowRight01Icon}
                                                className="size-4 shrink-0 text-muted-foreground/60"
                                            />
                                        </Link>
                                        <TargetMenu
                                            disabled={!editable}
                                            onEdit={() => showDialog({ kind: "edit", target })}
                                            onRemove={() => showDialog({ kind: "remove", target })}
                                        />
                                    </li>
                                ))}
                            </ul>
                        ) : (
                            <p className="px-5 py-8 text-sm text-muted-foreground">
                                {project.source
                                    ? "No deployment targets yet. Add one from a branch."
                                    : "Link a repository to add deployment targets."}
                            </p>
                        )}
                    </Panel>
                    <Panel title="Recent deployments">
                        {recent.length ? (
                            <ul className="divide-y">
                                {recent.map((deployment) => (
                                    <li key={deployment.id}>
                                        <Link
                                            to="/p/$project/$deployment"
                                            params={{
                                                project: project.id,
                                                deployment: deployment.id,
                                            }}
                                            className="flex items-center gap-3 px-5 py-4 transition-colors duration-150 hover:bg-accent/60"
                                        >
                                            <HugeiconsIcon
                                                icon={GitCommitIcon}
                                                className="size-4 shrink-0 text-muted-foreground"
                                            />
                                            <div className="min-w-0 flex-1">
                                                <p className="truncate text-sm font-medium">
                                                    {deployment.name}
                                                </p>
                                                <p className="truncate font-mono text-xs text-muted-foreground">
                                                    {deployment.commit?.slice(0, 12) ??
                                                        "Commit not recorded"}{" "}
                                                    · {deployment.git_ref ?? "Ref not recorded"}
                                                </p>
                                            </div>
                                            <time
                                                dateTime={deployment.deployed_at ?? undefined}
                                                className="shrink-0 text-xs text-muted-foreground"
                                            >
                                                {timeAgo(deployment.deployed_at ?? "")}
                                            </time>
                                        </Link>
                                    </li>
                                ))}
                            </ul>
                        ) : (
                            <p className="px-5 py-8 text-sm text-muted-foreground">
                                No deployments recorded yet.
                            </p>
                        )}
                    </Panel>
                </div>
                <div className="min-w-0 space-y-6">
                    <Panel
                        title="Repository"
                        action={
                            project.source && (
                                <Button
                                    size="sm"
                                    variant="ghost"
                                    disabled={!editable}
                                    title={reason}
                                    onClick={() => showDialog({ kind: "source" })}
                                >
                                    <HugeiconsIcon icon={EditIcon} /> Edit
                                </Button>
                            )
                        }
                    >
                        {project.source ? (
                            <Rows>
                                <Row label="Repository">
                                    <a
                                        href={project.source.repository}
                                        target="_blank"
                                        rel="noreferrer"
                                        className="break-all hover:underline"
                                    >
                                        {repoLabel(project.source.repository)}
                                    </a>
                                </Row>
                                <Row label="Default branch">
                                    {project.source.branch ?? "Not set"}
                                </Row>
                            </Rows>
                        ) : (
                            <div className="space-y-3 px-5 py-6 text-center">
                                <p className="text-sm text-muted-foreground">
                                    No repository is linked yet.
                                </p>
                                <Button
                                    size="sm"
                                    disabled={!editable}
                                    title={reason}
                                    onClick={() => showDialog({ kind: "source" })}
                                >
                                    Link a repository
                                </Button>
                            </div>
                        )}
                    </Panel>
                    {project.source && (
                        <Branches
                            project={project}
                            canAdd={editable}
                            onAdd={(branch) => showDialog({ kind: "add", branch })}
                        />
                    )}
                </div>
            </div>
            <SourceDialog
                project={project}
                open={dialogOpen && dialog?.kind === "source"}
                onOpenChange={close}
            />
            <TargetDialog
                project={project}
                target={dialog?.kind === "edit" ? dialog.target : undefined}
                initialBranch={dialog?.kind === "add" ? dialog.branch : undefined}
                open={dialogOpen && (dialog?.kind === "add" || dialog?.kind === "edit")}
                onOpenChange={close}
            />
            {dialog?.kind === "remove" && (
                <RemoveTargetDialog
                    project={project}
                    target={dialog.target}
                    open={dialogOpen}
                    onOpenChange={close}
                />
            )}
        </Page>
    );
}

function TargetMenu({
    disabled,
    onEdit,
    onRemove,
}: {
    disabled: boolean;
    onEdit: () => void;
    onRemove: () => void;
}) {
    return (
        <DropdownMenu>
            <DropdownMenuTrigger asChild>
                <Button
                    variant="ghost"
                    size="icon-sm"
                    disabled={disabled}
                    aria-label="Target actions"
                    className="text-muted-foreground"
                >
                    <HugeiconsIcon icon={MoreHorizontalCircle01Icon} />
                </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
                <DropdownMenuItem onSelect={onEdit}>
                    <HugeiconsIcon icon={EditIcon} /> Edit
                </DropdownMenuItem>
                <DropdownMenuItem variant="destructive" onSelect={onRemove}>
                    <HugeiconsIcon icon={Delete02Icon} /> Remove
                </DropdownMenuItem>
            </DropdownMenuContent>
        </DropdownMenu>
    );
}

/// Branches read live from the repository, each with the targets that already track it.
function Branches({
    project,
    canAdd,
    onAdd,
}: {
    project: ProjectData;
    canAdd: boolean;
    onAdd: (branch: string) => void;
}) {
    const branches = useBranches(project.source?.repository);
    const tracked = new Map<string, number>();
    for (const target of project.deployments) {
        if (target.git_ref) tracked.set(target.git_ref, (tracked.get(target.git_ref) ?? 0) + 1);
    }
    return (
        <Panel
            title="Branches"
            action={
                <Button
                    variant="ghost"
                    size="icon-sm"
                    aria-label="Refresh branches"
                    className="text-muted-foreground"
                    disabled={branches.isFetching}
                    onClick={() => void branches.refetch()}
                >
                    <HugeiconsIcon
                        icon={RepeatIcon}
                        className={branches.isFetching ? "animate-spin" : ""}
                    />
                </Button>
            }
        >
            {branches.isLoading ? (
                <p className="px-5 py-6 text-sm text-muted-foreground">Reading the repository…</p>
            ) : branches.error ? (
                <p role="alert" className="px-5 py-6 text-sm text-muted-foreground">
                    {branches.error.message}
                </p>
            ) : branches.data?.branches.length ? (
                <ul className="max-h-96 divide-y overflow-y-auto">
                    {branches.data.branches.map((branch) => {
                        const count = tracked.get(branch) ?? 0;
                        return (
                            <li key={branch} className="flex items-center gap-3 px-5 py-2.5">
                                <HugeiconsIcon
                                    icon={GitBranchIcon}
                                    className="size-3.5 shrink-0 text-muted-foreground"
                                />
                                <span className="min-w-0 flex-1 truncate font-mono text-xs">
                                    {branch}
                                </span>
                                {branch === branches.data?.default_branch && (
                                    <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
                                        default
                                    </span>
                                )}
                                {count > 0 && (
                                    <span className="text-xs text-muted-foreground">
                                        {count} {count === 1 ? "target" : "targets"}
                                    </span>
                                )}
                                <Button
                                    variant="ghost"
                                    size="icon-xs"
                                    aria-label={`Add target for ${branch}`}
                                    title="Add target"
                                    disabled={!canAdd}
                                    onClick={() => onAdd(branch)}
                                >
                                    <HugeiconsIcon icon={Add01Icon} />
                                </Button>
                            </li>
                        );
                    })}
                </ul>
            ) : (
                <p className="px-5 py-6 text-sm text-muted-foreground">
                    The repository has no branches.
                </p>
            )}
        </Panel>
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
