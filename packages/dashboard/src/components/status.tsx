import type { Status as StatusData, Tone } from "../lib/format.ts";

const tones: Record<Tone, string> = {
  success: "bg-success",
  warning: "bg-warning",
  danger: "bg-destructive",
  muted: "bg-muted-foreground/50",
};

export function Status({ status }: { status: StatusData }) {
  return (
    <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
      <span className={`inline-block size-2 shrink-0 rounded-full ${tones[status.tone]}`} />
      {status.label}
    </span>
  );
}
