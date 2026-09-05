import { ArrowRight, Database, HardDrive, Users } from "lucide-react";
import { useLogs, useStatus } from "@/lib/api";
import { clock } from "@/lib/format";
import { Panel, Row, Rows } from "@/routes/deployment";
import { useSession } from "@/lib/session";
import { Button } from "@/components/ui/button";

export function Functions() {
    return (
        <div className="grid gap-6 lg:grid-cols-2">
            <Panel title="Edge bundle">
                <Rows>
                    <Row label="Bundle">Not deployed</Row>
                    <Row label="Schema tables">—</Row>
                    <Row label="Queries · Mutations · Actions">—</Row>
                    <Row label="Crons">—</Row>
                </Rows>
            </Panel>
            <Panel title="Listeners">
                <ul className="divide-y text-sm">
                    {["ping", "login", "disconnect", "chat"].map((event) => (
                        <li key={event} className="flex items-center justify-between px-5 py-3">
                            <span className="font-mono text-xs">{event}</span>
                            <span className="text-xs text-muted-foreground">
                                Handled by the proxy default
                            </span>
                        </li>
                    ))}
                </ul>
                <p className="border-t px-5 py-3 text-xs text-muted-foreground">
                    These are the four events an edge bundle can take over. Until one is deployed
                    the proxy answers them itself: status from the MOTD, login into limbo.
                </p>
            </Panel>
        </div>
    );
}

export function Sessions() {
    const status = useStatus();
    const stages = [
        { title: "Edge", count: 1, detail: status.data?.minecraft_bind ?? "…" },
        {
            title: "Runtimes",
            count: 0,
            detail: status.data?.reconciliation ? "Reconciling" : "No runtime connected",
        },
        { title: "Sessions", count: 0, detail: "Waiting for a runtime" },
    ];
    return (
        <div className="space-y-6">
            <div className="grid items-stretch gap-3 sm:grid-cols-[1fr_auto_1fr_auto_1fr]">
                {stages.map((stage, index) => (
                    <FlowStage key={stage.title} {...stage} last={index === stages.length - 1} />
                ))}
            </div>
            <p className="text-sm text-muted-foreground">
                Players connect to the edge, the controller places them on a runtime, and the
                runtime hosts sessions in a JVM. Players who log in now are held in limbo at the
                edge because nothing is downstream yet.
            </p>
        </div>
    );
}

function FlowStage({
    title,
    count,
    detail,
    last,
}: {
    title: string;
    count: number;
    detail: string;
    last: boolean;
}) {
    return (
        <>
            <div className="rounded-lg border bg-card px-5 py-4">
                <p className="text-xs text-muted-foreground">{title}</p>
                <p className="mt-1 text-2xl font-semibold tabular-nums tracking-tight">{count}</p>
                <p className="mt-1 truncate font-mono text-xs text-muted-foreground">{detail}</p>
            </div>
            {!last && (
                <ArrowRight className="hidden size-4 self-center text-muted-foreground/60 sm:block" />
            )}
        </>
    );
}

export function Players() {
    const status = useStatus();
    const logs = useLogs();
    const online = status.data?.connections ?? 0;
    const max = status.data?.max_connections ?? 0;
    const logins = (logs.data ?? [])
        .filter((entry) => entry.message.startsWith("authenticated player"))
        .map((entry) => ({
            seq: entry.seq,
            time: entry.time_ms,
            name: /username=(\S+)/.exec(entry.message)?.[1] ?? "?",
        }))
        .reverse()
        .slice(0, 25);

    return (
        <div className="grid gap-6 lg:grid-cols-[1fr_1.4fr]">
            <Panel title="Connections">
                <div className="space-y-4 p-5">
                    <p className="text-4xl font-semibold tabular-nums tracking-tight">
                        {online}
                        <span className="text-base font-normal text-muted-foreground">
                            {" "}
                            of {max}
                        </span>
                    </p>
                    <div className="h-1.5 overflow-hidden rounded-full bg-muted">
                        <div
                            className="h-full rounded-full bg-foreground transition-[width]"
                            style={{
                                width: `${max ? Math.max((online / max) * 100, online ? 1 : 0) : 0}%`,
                            }}
                        />
                    </div>
                    <p className="text-xs text-muted-foreground">
                        Open sockets on the Minecraft listener, including players waiting in limbo.
                        Placement is not enabled, so every login stays here.
                    </p>
                </div>
            </Panel>
            <Panel title="Recent logins">
                {logins.length === 0 ? (
                    <p className="flex items-center gap-2 px-5 py-6 text-sm text-muted-foreground">
                        <Users className="size-4" />
                        No one has logged in since the backend started.
                    </p>
                ) : (
                    <ul className="divide-y text-sm">
                        {logins.map((login) => (
                            <li key={login.seq} className="flex items-center gap-3 px-5 py-2.5">
                                <img
                                    src={`https://mc-heads.net/avatar/${encodeURIComponent(login.name)}/24`}
                                    alt=""
                                    width={24}
                                    height={24}
                                    className="rounded-[3px]"
                                />
                                <span className="flex-1 font-medium">{login.name}</span>
                                <time className="font-mono text-xs text-muted-foreground">
                                    {clock.format(login.time)}
                                </time>
                            </li>
                        ))}
                    </ul>
                )}
            </Panel>
        </div>
    );
}

export function Assets() {
    const backends = [
        {
            icon: HardDrive,
            title: "Local filesystem",
            detail: "Objects on this box, keyed by SHA-256. Simplest for a single machine.",
        },
        {
            icon: Database,
            title: "S3-compatible bucket",
            detail: "Operator-supplied bucket for durability and multi-machine runtimes.",
        },
    ];
    return (
        <div className="space-y-6">
            <div className="grid gap-4 sm:grid-cols-2">
                {backends.map((backend) => (
                    <div key={backend.title} className="rounded-lg border bg-card px-5 py-4">
                        <div className="flex items-center justify-between">
                            <backend.icon className="size-4 text-muted-foreground" />
                            <span className="text-xs text-muted-foreground">Not configured</span>
                        </div>
                        <p className="mt-3 font-medium">{backend.title}</p>
                        <p className="mt-1 text-sm text-muted-foreground">{backend.detail}</p>
                    </div>
                ))}
            </div>
            <Panel title="Manifest">
                <Rows>
                    <Row label="chunk.assets.json">Not published</Row>
                    <Row label="Pinned objects">0</Row>
                </Rows>
                <p className="border-t px-5 py-3 text-xs text-muted-foreground">
                    Deployments pin immutable objects by content hash. Bytes live in the store
                    above; only the manifest is committed.
                </p>
            </Panel>
        </div>
    );
}

export function Settings() {
    const status = useStatus();
    const { disconnect } = useSession();
    const data = status.data;
    return (
        <div className="space-y-6">
            <Panel title="Listeners">
                <Rows>
                    <Row label="Minecraft">{data?.minecraft_bind ?? "…"}</Row>
                    <Row label="Management">{data?.management_bind ?? "…"}</Row>
                    <Row label="Max connections">{data?.max_connections ?? "…"}</Row>
                    <Row label="MOTD">{data?.motd ?? "…"}</Row>
                </Rows>
            </Panel>
            <Panel title="Files">
                <Rows>
                    <Row label="Dashboard">{data?.dashboard_dir ?? "…"}</Row>
                    <Row label="Projects">{data?.projects_file ?? "none"}</Row>
                </Rows>
            </Panel>
            <Panel title="Operator access">
                <div className="flex flex-wrap items-center justify-between gap-4 px-5 py-4 text-sm">
                    <div>
                        <p>Management token</p>
                        <p className="text-xs text-muted-foreground">
                            Set with CHUNK_MANAGEMENT_TOKEN on the backend. Held in this tab's
                            memory only.
                        </p>
                    </div>
                    <Button variant="outline" size="sm" onClick={disconnect}>
                        Disconnect
                    </Button>
                </div>
            </Panel>
        </div>
    );
}
