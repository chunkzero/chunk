import { ArrowDownToLine, Pause, Play } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { type LogEntry, useLogs } from "@/lib/api";
import { clock } from "@/lib/format";

const levels = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"] as const;
const levelTone: Record<string, string> = {
    ERROR: "bg-destructive",
    WARN: "bg-warning",
    INFO: "bg-link",
    DEBUG: "bg-muted-foreground/60",
    TRACE: "bg-muted-foreground/30",
};

export function Logs() {
    const logs = useLogs();
    const [filter, setFilter] = useState("");
    const [hidden, setHidden] = useState<Set<string>>(new Set());
    const [following, setFollowing] = useState(true);
    const [frozen, setFrozen] = useState<LogEntry[] | null>(null);
    const list = useRef<HTMLOListElement>(null);

    const source = frozen ?? logs.data?.entries ?? [];
    const needle = filter.toLowerCase();
    const entries = source.filter(
        (entry) =>
            !hidden.has(entry.level) &&
            (needle === "" || `${entry.target} ${entry.message}`.toLowerCase().includes(needle)),
    );

    useEffect(() => {
        if (following && list.current) list.current.scrollTop = list.current.scrollHeight;
    }, [entries.at(-1)?.seq, logs.data?.stream, following]);

    return (
        <div className="overflow-hidden rounded-lg border bg-card">
            {logs.error && (
                <p role="alert" className="p-4 text-sm text-destructive">
                    Logs could not refresh. Retrying…
                </p>
            )}
            {logs.data?.truncated && (
                <p className="px-4 py-2 text-xs text-muted-foreground">
                    Earlier log entries are no longer available.
                </p>
            )}
            <div className="flex flex-wrap items-center gap-2 border-b px-3 py-2">
                <Input
                    value={filter}
                    onChange={(event) => setFilter(event.target.value)}
                    placeholder="Filter by target or message"
                    className="h-8 max-w-xs font-mono text-xs"
                />
                <div className="flex gap-1">
                    {levels.map((level) => (
                        <button
                            key={level}
                            type="button"
                            onClick={() =>
                                setHidden((current) => {
                                    const next = new Set(current);
                                    if (next.has(level)) next.delete(level);
                                    else next.add(level);
                                    return next;
                                })
                            }
                            className={`flex items-center gap-1.5 rounded-md border px-2 py-1 font-mono text-[11px] ${hidden.has(level) ? "text-muted-foreground/50 line-through" : ""}`}
                        >
                            <span className={`size-1.5 rounded-full ${levelTone[level]}`} />
                            {level}
                        </button>
                    ))}
                </div>
                <span className="ml-auto text-xs text-muted-foreground tabular-nums">
                    {entries.length} of {source.length}
                </span>
                <Button
                    variant="ghost"
                    size="xs"
                    onClick={() => setFrozen(frozen ? null : (logs.data?.entries ?? []))}
                    aria-pressed={frozen !== null}
                >
                    {frozen ? <Play /> : <Pause />}
                    {frozen ? "Resume" : "Pause"}
                </Button>
                <Button
                    variant="ghost"
                    size="xs"
                    onClick={() => setFollowing((value) => !value)}
                    aria-pressed={following}
                    className={following ? "" : "text-muted-foreground"}
                >
                    <ArrowDownToLine />
                    Follow
                </Button>
            </div>
            <ol
                ref={list}
                onScroll={(event) => {
                    const element = event.currentTarget;
                    const atBottom =
                        element.scrollHeight - element.scrollTop - element.clientHeight < 8;
                    if (!atBottom && following) setFollowing(false);
                }}
                className="h-[60vh] overflow-y-auto bg-[oklch(0.99_0.002_250)] font-mono text-xs leading-6"
            >
                {entries.length === 0 && (
                    <li className="px-4 py-6 text-muted-foreground">
                        {source.length === 0
                            ? logs.isPending
                                ? "Loading logs…"
                                : logs.error
                                  ? "Logs unavailable."
                                  : "No log lines yet."
                            : "Nothing matches the current filter."}
                    </li>
                )}
                {entries.map((entry) => (
                    <li key={entry.seq} className="flex gap-3 px-4 hover:bg-accent/60">
                        <time className="shrink-0 text-muted-foreground tabular-nums">
                            {clock.format(entry.time_ms)}
                        </time>
                        <span className="flex w-12 shrink-0 items-center gap-1.5 text-muted-foreground">
                            <span className={`size-1.5 rounded-full ${levelTone[entry.level]}`} />
                            {entry.level}
                        </span>
                        <span className="shrink-0 text-muted-foreground">
                            {entry.target.replace(/^chunk_/, "")}
                        </span>
                        <span className="min-w-0 break-words">{entry.message}</span>
                    </li>
                ))}
            </ol>
        </div>
    );
}
