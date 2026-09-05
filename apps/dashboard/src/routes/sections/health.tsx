import { Check, Copy, GitBranch, GitCommitHorizontal, Minus } from "lucide-react";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { type Status, useStatus, useSystem } from "@/lib/api";
import { bytes, duration, timeAgo } from "@/lib/format";
import { Panel, Row, Rows, useDeployment } from "@/routes/deployment";

const capabilities: {
    key: keyof Pick<Status, "functions" | "reconciliation" | "asset_uploads">;
    label: string;
    hint: string;
}[] = [
    { key: "functions", label: "Edge functions", hint: "QuickJS runtime and reactive database" },
    { key: "reconciliation", label: "Session runtime", hint: "Placement and JVM supervision" },
    { key: "asset_uploads", label: "Asset storage", hint: "Immutable objects on disk or S3" },
];

export function Health() {
    const status = useStatus();
    const system = useSystem();
    const { deployment, project } = useDeployment();
    const data = status.data;
    const machine = system.data;

    return (
        <div className="space-y-6">
            <div className="grid divide-y rounded-lg border bg-card sm:grid-cols-4 sm:divide-x sm:divide-y-0">
                <Vital label="Uptime">
                    {data ? <Uptime seconds={data.uptime_seconds} /> : "…"}
                </Vital>
                <Vital label="Connections">
                    {data ? (
                        <>
                            {data.connections}
                            <span className="text-base font-normal text-muted-foreground">
                                {" "}
                                / {data.max_connections}
                            </span>
                        </>
                    ) : (
                        "…"
                    )}
                </Vital>
                <Vital label="Minecraft" copy={data?.minecraft_bind}>
                    {data?.minecraft_bind ?? "…"}
                </Vital>
                <Vital label="Management" copy={data?.management_bind}>
                    {data?.management_bind ?? "…"}
                </Vital>
            </div>

            <div className="grid gap-6 lg:grid-cols-[1.4fr_1fr]">
                <div className="space-y-6">
                    <Panel title="Server list preview">
                        <div className="p-5">
                            <ServerListEntry
                                name={project?.name ?? "chunk"}
                                motd={data?.motd ?? ""}
                                online={data?.connections ?? 0}
                                max={data?.max_connections ?? 0}
                            />
                            <p className="mt-3 text-xs text-muted-foreground">
                                What players see when they add{" "}
                                {data?.minecraft_bind ?? "this server"} to their server list.
                            </p>
                        </div>
                    </Panel>
                    <Panel title="Deployment">
                        <Rows>
                            <Row label="Ref">
                                {deployment?.git_ref ? (
                                    <span className="inline-flex items-center gap-1">
                                        <GitBranch className="size-3" />
                                        {deployment.git_ref}
                                    </span>
                                ) : (
                                    "—"
                                )}
                            </Row>
                            <Row label="Commit">
                                {deployment?.commit ? (
                                    <span className="inline-flex items-center gap-1">
                                        <GitCommitHorizontal className="size-3" />
                                        {deployment.commit.slice(0, 12)}
                                    </span>
                                ) : (
                                    "—"
                                )}
                            </Row>
                            <Row label="Deployed">
                                {deployment?.deployed_at ? timeAgo(deployment.deployed_at) : "—"}
                            </Row>
                            <Row label="Backend">chunk v{data?.version ?? "…"}</Row>
                        </Rows>
                    </Panel>
                </div>
                <div className="space-y-6">
                    <Panel
                        title="Machine"
                        action={
                            <span className="font-mono text-xs text-muted-foreground">
                                {machine?.hostname ?? ""}
                            </span>
                        }
                    >
                        {machine ? (
                            <div className="space-y-5 p-5 text-sm">
                                <Meter
                                    label="Memory"
                                    used={machine.memory_used}
                                    total={machine.memory_total}
                                    detail={`${bytes(machine.memory_available)} available`}
                                />
                                {machine.swap_total > 0 && (
                                    <Meter
                                        label="Swap"
                                        used={machine.swap_used}
                                        total={machine.swap_total}
                                    />
                                )}
                                <Meter
                                    label="CPU"
                                    used={machine.cpu_percent}
                                    total={100}
                                    value={`${machine.cpu_percent.toFixed(0)}%`}
                                    detail={`${machine.cpus} cores · load ${machine.load_average.map((value) => value.toFixed(2)).join(" ")}`}
                                />
                                <div className="flex items-center justify-between border-t pt-4 text-xs">
                                    <span className="text-muted-foreground">This process</span>
                                    <span className="font-mono">
                                        {bytes(machine.process_memory)} ·{" "}
                                        {machine.process_cpu_percent.toFixed(1)}% CPU
                                    </span>
                                </div>
                                {machine.os && (
                                    <p className="text-xs text-muted-foreground">{machine.os}</p>
                                )}
                            </div>
                        ) : (
                            <p className="p-5 text-sm text-muted-foreground">…</p>
                        )}
                    </Panel>
                    <Panel title="Capabilities">
                        <ul className="divide-y text-sm">
                            {capabilities.map((capability) => {
                                const enabled = data?.[capability.key] ?? false;
                                return (
                                    <li
                                        key={capability.key}
                                        className="flex items-start gap-3 px-5 py-3"
                                    >
                                        <span
                                            className={`mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-full border ${enabled ? "border-success bg-success/10 text-success" : "text-muted-foreground/60"}`}
                                        >
                                            {enabled ? (
                                                <Check className="size-3" />
                                            ) : (
                                                <Minus className="size-3" />
                                            )}
                                        </span>
                                        <span className="flex-1">
                                            <span
                                                className={enabled ? "" : "text-muted-foreground"}
                                            >
                                                {capability.label}
                                            </span>
                                            <span className="block text-xs text-muted-foreground">
                                                {capability.hint}
                                            </span>
                                        </span>
                                    </li>
                                );
                            })}
                        </ul>
                    </Panel>
                </div>
            </div>
        </div>
    );
}

function Meter({
    label,
    used,
    total,
    value,
    detail,
}: {
    label: string;
    used: number;
    total: number;
    value?: string;
    detail?: string;
}) {
    const percent = total > 0 ? Math.min((used / total) * 100, 100) : 0;
    return (
        <div className="space-y-1.5">
            <div className="flex items-baseline justify-between">
                <span>{label}</span>
                <span className="font-mono text-xs">
                    {value ?? `${bytes(used)} / ${bytes(total)}`}
                </span>
            </div>
            <div className="h-1.5 overflow-hidden rounded-full bg-muted">
                <div
                    className={`h-full rounded-full transition-[width] ${percent > 90 ? "bg-destructive" : percent > 75 ? "bg-warning" : "bg-foreground"}`}
                    style={{ width: `${percent}%` }}
                />
            </div>
            {detail && <p className="text-xs text-muted-foreground">{detail}</p>}
        </div>
    );
}

function Vital({
    label,
    copy,
    children,
}: {
    label: string;
    copy?: string;
    children: React.ReactNode;
}) {
    return (
        <div className="flex items-start justify-between gap-2 px-5 py-4">
            <div className="min-w-0">
                <p className="text-xs text-muted-foreground">{label}</p>
                <p
                    className={`mt-1 truncate font-mono font-semibold tabular-nums tracking-tight ${copy ? "pt-1 text-sm" : "text-xl"}`}
                >
                    {children}
                </p>
            </div>
            {copy && <CopyButton value={copy} />}
        </div>
    );
}

function CopyButton({ value }: { value: string }) {
    const [copied, setCopied] = useState(false);
    return (
        <Button
            variant="ghost"
            size="icon-xs"
            className="text-muted-foreground"
            aria-label="Copy"
            onClick={() => {
                void navigator.clipboard.writeText(value);
                setCopied(true);
                setTimeout(() => setCopied(false), 1200);
            }}
        >
            {copied ? <Check className="text-success" /> : <Copy />}
        </Button>
    );
}

/// Ticks locally between status polls so uptime reads as a live clock.
function Uptime({ seconds }: { seconds: number }) {
    const [offset, setOffset] = useState(0);
    useEffect(() => {
        setOffset(0);
        const timer = setInterval(() => setOffset((value) => value + 1), 1000);
        return () => clearInterval(timer);
    }, [seconds]);
    return <>{duration(seconds + offset)}</>;
}

function ServerListEntry({
    name,
    motd,
    online,
    max,
}: {
    name: string;
    motd: string;
    online: number;
    max: number;
}) {
    return (
        <div className="flex items-center gap-3 rounded-md bg-[oklch(0.2_0.003_250)] p-2 text-[oklch(0.97_0_0)] shadow-inner">
            <div className="flex size-16 shrink-0 items-center justify-center rounded-sm bg-[oklch(0.3_0.003_250)]">
                <span className="size-6 rounded-[3px] bg-[oklch(0.97_0_0)]" />
            </div>
            <div className="min-w-0 flex-1 font-mono text-sm leading-6">
                <div className="flex items-center justify-between gap-4">
                    <span className="truncate">{name}</span>
                    <span className="flex items-center gap-2 text-[oklch(0.7_0_0)]">
                        {online}/{max}
                        <SignalBars />
                    </span>
                </div>
                <p className="truncate text-[oklch(0.75_0_0)]">{motd || " "}</p>
            </div>
        </div>
    );
}

function SignalBars() {
    return (
        <span className="flex items-end gap-px" aria-hidden>
            {[2, 3, 4, 5, 6].map((height) => (
                <span
                    key={height}
                    className="w-1 bg-[oklch(0.75_0.17_136)]"
                    style={{ height: `${height * 2}px` }}
                />
            ))}
        </span>
    );
}
