import { timestampDate } from "@bufbuild/protobuf/wkt";
import { ArrowDownToLineIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useParams } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";

import { Button } from "../../components/ui/button.tsx";
import { Input } from "../../components/ui/input.tsx";
import { LogSeverity } from "../../gen/chunk/management/v1/common_pb.ts";
import { clock, severityLabels, sourceLabels } from "../../lib/format.ts";
import { type LogStream, useLogStream } from "../../lib/logs.ts";

const severityTone: Record<LogSeverity, string> = {
  [LogSeverity.UNSPECIFIED]: "bg-muted-foreground/30",
  [LogSeverity.DEBUG]: "bg-muted-foreground/60",
  [LogSeverity.INFO]: "bg-link",
  [LogSeverity.WARN]: "bg-warning",
  [LogSeverity.ERROR]: "bg-destructive",
};

export function Logs() {
  const { environment } = useParams({ from: "/p/$project/$environment" });
  const [follow, setFollow] = useState(true);
  const stream = useLogStream(environment, follow);
  const [filter, setFilter] = useState("");
  const needle = filter.toLowerCase();
  const entries = needle
    ? stream.entries.filter((entry) => entry.message.toLowerCase().includes(needle))
    : stream.entries;

  const list = useRef<HTMLOListElement>(null);
  const pinned = useRef(true);
  useEffect(() => {
    if (pinned.current && list.current) list.current.scrollTop = list.current.scrollHeight;
  }, [entries]);

  return (
    <div className="overflow-hidden rounded-lg border bg-card">
      <div className="flex flex-wrap items-center gap-2 border-b px-3 py-2">
        <Input
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
          placeholder="Filter messages"
          className="h-8 max-w-xs font-mono text-xs"
        />
        <span className="ml-auto text-xs text-muted-foreground tabular-nums">
          {entries.length} of {stream.entries.length}
        </span>
        <Button
          variant="ghost"
          size="xs"
          onClick={() => setFollow((value) => !value)}
          aria-pressed={follow}
          className={follow ? "" : "text-muted-foreground"}
        >
          <HugeiconsIcon icon={ArrowDownToLineIcon} />
          Follow
        </Button>
      </div>
      {stream.error && (
        <p role="alert" className="flex items-center gap-3 border-b px-4 py-2 text-sm text-destructive">
          {stream.error}
          <Button size="xs" variant="outline" onClick={stream.retry}>
            Retry
          </Button>
        </p>
      )}
      <ol
        ref={list}
        onScroll={(event) => {
          const element = event.currentTarget;
          pinned.current = element.scrollHeight - element.scrollTop - element.clientHeight < 8;
        }}
        className="h-[60vh] overflow-y-auto bg-background py-1 font-mono text-xs leading-6"
      >
        {entries.length === 0 && <li className="px-4 py-6 text-muted-foreground">{placeholder(stream, follow)}</li>}
        {entries.map((entry) => (
          <li key={`${entry.instanceId}:${entry.sequence}`} className="flex gap-3 px-4 hover:bg-accent/60">
            <time className="shrink-0 text-muted-foreground tabular-nums">
              {entry.time ? clock.format(timestampDate(entry.time)) : ""}
            </time>
            <span className="flex w-14 shrink-0 items-center gap-1.5 text-muted-foreground">
              <span className={`size-1.5 shrink-0 rounded-full ${severityTone[entry.severity]}`} />
              {severityLabels[entry.severity]}
            </span>
            <span className="w-14 shrink-0 text-muted-foreground">{sourceLabels[entry.source]}</span>
            <span className="min-w-0 break-words whitespace-pre-wrap">{entry.message}</span>
          </li>
        ))}
      </ol>
    </div>
  );
}

function placeholder(stream: LogStream, follow: boolean) {
  if (stream.entries.length > 0) return "Nothing matches the filter.";
  if (stream.loading) return follow ? "Waiting for log lines…" : "Loading logs…";
  return "No log lines.";
}
