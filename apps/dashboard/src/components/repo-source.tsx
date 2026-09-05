import type { Project } from "@/lib/api";
import { repoLabel } from "@/lib/format";
import { HugeiconsIcon } from "@hugeicons/react";
import { GitBranchIcon, GitCommitIcon, GithubIcon } from "@hugeicons/core-free-icons";

export function RepoSource({ source }: { source: Project["source"] }) {
    if (!source) return <span className="text-sm text-muted-foreground">No linked repository</span>;
    const href = /^https?:/.test(source.repository) ? source.repository : undefined;
    return (
        <span className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted-foreground">
            <a
                href={href}
                target="_blank"
                rel="noreferrer"
                className="flex items-center gap-1.5 font-mono text-xs hover:text-foreground"
                onClick={(event) => event.stopPropagation()}
            >
                <HugeiconsIcon icon={GithubIcon} className="size-3.5" />
                {repoLabel(source.repository)}
            </a>
            {source.branch && (
                <span className="flex items-center gap-1 font-mono text-xs">
                    <HugeiconsIcon icon={GitBranchIcon} className="size-3.5" />
                    {source.branch}
                </span>
            )}
        </span>
    );
}

export function Commit({ commit, gitRef }: { commit: string | null; gitRef: string | null }) {
    if (!commit && !gitRef) return <span className="text-muted-foreground">No build recorded</span>;
    return (
        <span className="flex items-center gap-2 font-mono text-xs">
            {gitRef && (
                <span className="flex items-center gap-1">
                    <HugeiconsIcon icon={GitBranchIcon} className="size-3.5" />
                    {gitRef}
                </span>
            )}
            {commit && (
                <span className="flex items-center gap-1 text-muted-foreground">
                    <HugeiconsIcon icon={GitCommitIcon} className="size-3.5" />
                    {commit.slice(0, 7)}
                </span>
            )}
        </span>
    );
}
