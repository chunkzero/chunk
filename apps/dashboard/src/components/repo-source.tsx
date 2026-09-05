import { GitBranch, GitCommitHorizontal } from "lucide-react";
import type { Project } from "@/lib/api";
import { repoLabel } from "@/lib/format";

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
                <GitHubMark />
                {repoLabel(source.repository)}
            </a>
            {source.branch && (
                <span className="flex items-center gap-1 font-mono text-xs">
                    <GitBranch className="size-3.5" />
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
                    <GitBranch className="size-3.5" />
                    {gitRef}
                </span>
            )}
            {commit && (
                <span className="flex items-center gap-1 text-muted-foreground">
                    <GitCommitHorizontal className="size-3.5" />
                    {commit.slice(0, 7)}
                </span>
            )}
        </span>
    );
}

function GitHubMark() {
    return (
        <svg viewBox="0 0 16 16" className="size-3.5 fill-current" aria-hidden>
            <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8" />
        </svg>
    );
}
