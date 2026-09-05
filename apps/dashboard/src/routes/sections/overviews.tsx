import { useState } from "react";
import { Folder, History, Upload } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Panel, Row, Rows } from "@/components/panel";
import { useDeployment } from "@/routes/deployment";
import { timeAgo } from "@/lib/format";

export function Application() {
    const { deployment } = useDeployment();
    return (
        <div className="space-y-6">
            <div className="flex items-center justify-between gap-4">
                <h1 className="text-xl font-semibold">{deployment?.name}</h1>
                <span className="rounded-md bg-muted px-2.5 py-1 text-xs">
                    {deployment?.environment}
                </span>
            </div>
            <div className="grid gap-6 md:grid-cols-2">
                <Panel title="Deployment">
                    <Rows>
                        <Row label="Branch">{deployment?.git_ref ?? "—"}</Row>
                        <Row label="Commit">{deployment?.commit?.slice(0, 12) ?? "—"}</Row>
                        <Row label="Deployed">
                            {deployment?.deployed_at ? timeAgo(deployment.deployed_at) : "—"}
                        </Row>
                        <Row label="Health">Not available</Row>
                    </Rows>
                </Panel>
                <Panel title="Sessions">
                    <Placeholder title="Sessions are not available yet" />
                </Panel>
            </div>
        </div>
    );
}

export function Players() {
    return (
        <div className="space-y-6">
            <h1 className="text-xl font-semibold">Players</h1>
            <Panel title="Current connections">
                <EmptyTable
                    columns={["Player", "Session", "Connected", "Status"]}
                    message="Player connections are not available yet."
                />
            </Panel>
            <Panel title="Player log">
                <EmptyTable
                    columns={["Time", "Player", "Event", "Session"]}
                    message="Player activity is not available yet."
                />
            </Panel>
        </div>
    );
}

export function Assets() {
    const [view, setView] = useState<"files" | "versions">("files");
    return (
        <div className="space-y-5">
            <div className="flex items-center justify-between gap-4">
                <h1 className="text-xl font-semibold">Assets</h1>
                <Button size="sm" disabled title="Asset uploads are not available yet">
                    <Upload /> Upload files
                </Button>
            </div>
            <div className="overflow-hidden rounded-lg border bg-card">
                <div className="flex flex-wrap items-center gap-2 border-b p-3">
                    <div className="flex gap-1" aria-label="Asset views">
                        <Button
                            size="sm"
                            variant={view === "files" ? "secondary" : "ghost"}
                            aria-pressed={view === "files"}
                            onClick={() => setView("files")}
                        >
                            <Folder /> Files
                        </Button>
                        <Button
                            size="sm"
                            variant={view === "versions" ? "secondary" : "ghost"}
                            aria-pressed={view === "versions"}
                            onClick={() => setView("versions")}
                        >
                            <History /> Versions
                        </Button>
                    </div>
                    <span className="ml-auto text-xs text-muted-foreground">
                        No published version
                    </span>
                </div>
                {view === "files" ? (
                    <>
                        <div className="flex items-center gap-2 border-b px-5 py-3 text-sm">
                            <Folder className="size-4 text-muted-foreground" /> All files
                        </div>
                        <EmptyTable
                            columns={["Name", "Size", "Last changed", "Version"]}
                            message="Asset files are not available yet."
                        />
                    </>
                ) : (
                    <EmptyTable
                        columns={["Version", "Published", "Files", "Status"]}
                        message="Published versions will appear here as file snapshots."
                    />
                )}
            </div>
        </div>
    );
}

function Placeholder({ title }: { title: string }) {
    return <p className="px-5 py-10 text-center text-sm text-muted-foreground">{title}</p>;
}

function EmptyTable({ columns, message }: { columns: string[]; message: string }) {
    return (
        <div className="overflow-x-auto">
            <table className="w-full text-left text-sm">
                <thead>
                    <tr className="border-b">
                        {columns.map((column) => (
                            <th
                                key={column}
                                className="px-5 py-3 text-xs font-medium text-muted-foreground"
                            >
                                {column}
                            </th>
                        ))}
                    </tr>
                </thead>
                <tbody>
                    <tr>
                        <td colSpan={columns.length}>
                            <Placeholder title={message} />
                        </td>
                    </tr>
                </tbody>
            </table>
        </div>
    );
}
