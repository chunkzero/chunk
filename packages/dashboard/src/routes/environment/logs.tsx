import { timestampDate } from "@bufbuild/protobuf/wkt";
import { ArrowDownToLineIcon } from "@hugeicons/core-free-icons";
import * as stylex from "@stylexjs/stylex";
import { useParams } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";

import { Button } from "../../components/ui/button.tsx";
import { Input } from "../../components/ui/input.tsx";
import { LogSeverity } from "../../gen/chunk/management/v1/common_pb.ts";
import { clock, severityLabels, sourceLabels } from "../../lib/format.ts";
import { type LogStream, useLogStream } from "../../lib/logs.ts";
import { colors, fonts, fontSizes, lineHeights, radii, space } from "../../tokens.stylex.ts";

const md = "@media (min-width: 48rem)";

const styles = stylex.create({
  logs: {
    overflow: "hidden",
    borderWidth: "1px",
    borderStyle: "solid",
    borderColor: colors.border,
    borderRadius: radii.lg,
    backgroundColor: colors.card,
  },
  toolbar: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: space.s2,
    paddingInline: space.s3,
    paddingBlock: space.s2,
    borderBottomWidth: "1px",
    borderBottomStyle: "solid",
    borderBottomColor: colors.border,
  },
  filter: {
    maxWidth: "20rem",
    fontFamily: fonts.mono,
    fontSize: { default: fontSizes.xs, [md]: fontSizes.sm },
    lineHeight: { default: lineHeights.xs, [md]: lineHeights.sm },
  },
  count: {
    marginLeft: "auto",
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
    color: colors.mutedForeground,
    fontVariantNumeric: "tabular-nums",
  },
  paused: { color: { default: colors.mutedForeground, ":hover": colors.accentForeground } },
  error: {
    display: "flex",
    alignItems: "center",
    gap: space.s3,
    paddingInline: space.s4,
    paddingBlock: space.s2,
    borderBottomWidth: "1px",
    borderBottomStyle: "solid",
    borderBottomColor: colors.border,
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
    color: colors.destructive,
  },
  list: {
    height: "60vh",
    overflowY: "auto",
    paddingBlock: space.s1,
    backgroundColor: colors.background,
    fontFamily: fonts.mono,
    fontSize: fontSizes.xs,
    lineHeight: "1.65rem",
  },
  placeholder: { paddingInline: space.s4, paddingBlock: space.s6, color: colors.mutedForeground },
  entry: {
    display: "flex",
    gap: space.s3,
    paddingInline: space.s4,
    backgroundColor: { default: null, ":hover": `color-mix(in oklab, ${colors.accent} 60%, transparent)` },
  },
  time: { flexShrink: 0, color: colors.mutedForeground, fontVariantNumeric: "tabular-nums" },
  severity: {
    display: "flex",
    flexShrink: 0,
    alignItems: "center",
    gap: space.s1_5,
    width: "3.85rem",
    color: colors.mutedForeground,
  },
  dot: { flexShrink: 0, width: space.s1_5, height: space.s1_5, borderRadius: "9999px" },
  source: { flexShrink: 0, width: "3.85rem", color: colors.mutedForeground },
  message: { minWidth: 0, overflowWrap: "break-word", whiteSpace: "pre-wrap" },
});

const tones = stylex.create({
  unspecified: { backgroundColor: `color-mix(in oklab, ${colors.mutedForeground} 30%, transparent)` },
  debug: { backgroundColor: `color-mix(in oklab, ${colors.mutedForeground} 60%, transparent)` },
  info: { backgroundColor: colors.link },
  warn: { backgroundColor: colors.warning },
  error: { backgroundColor: colors.destructive },
});

const severityTone: Record<LogSeverity, stylex.StyleXStyles> = {
  [LogSeverity.UNSPECIFIED]: tones.unspecified,
  [LogSeverity.DEBUG]: tones.debug,
  [LogSeverity.INFO]: tones.info,
  [LogSeverity.WARN]: tones.warn,
  [LogSeverity.ERROR]: tones.error,
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
    <div {...stylex.props(styles.logs)}>
      <div {...stylex.props(styles.toolbar)}>
        <Input value={filter} onValueChange={setFilter} placeholder="Filter messages" style={styles.filter} />
        <span {...stylex.props(styles.count)}>
          {entries.length} of {stream.entries.length}
        </span>
        <Button
          variant="ghost"
          size="xs"
          onClick={() => setFollow((value) => !value)}
          aria-pressed={follow}
          icon={ArrowDownToLineIcon}
          style={!follow && styles.paused}
        >
          Follow
        </Button>
      </div>
      {stream.error && (
        <p role="alert" {...stylex.props(styles.error)}>
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
        {...stylex.props(styles.list)}
      >
        {entries.length === 0 && <li {...stylex.props(styles.placeholder)}>{placeholder(stream, follow)}</li>}
        {entries.map((entry) => (
          <li key={`${entry.instanceId}:${entry.sequence}`} {...stylex.props(styles.entry)}>
            <time {...stylex.props(styles.time)}>{entry.time ? clock.format(timestampDate(entry.time)) : ""}</time>
            <span {...stylex.props(styles.severity)}>
              <span {...stylex.props(styles.dot, severityTone[entry.severity])} />
              {severityLabels[entry.severity]}
            </span>
            <span {...stylex.props(styles.source)}>{sourceLabels[entry.source]}</span>
            <span {...stylex.props(styles.message)}>{entry.message}</span>
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
