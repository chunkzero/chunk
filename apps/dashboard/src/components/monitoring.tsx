import { useStatus, useSystem } from "@/lib/api";
import { bytes, duration } from "@/lib/format";

export function Monitoring() {
    const status = useStatus();
    const system = useSystem();
    const data = status.data;
    const machine = system.data;
    const metrics = [
        {
            label: "Total connections",
            value: data?.connections.toLocaleString() ?? "—",
            detail: data ? `of ${data.max_connections.toLocaleString()} maximum` : "",
        },
        {
            label: "CPU",
            value: machine ? `${machine.cpu_percent.toFixed(0)}%` : "—",
            detail: machine ? `${machine.cpus} cores` : "",
        },
        {
            label: "Memory",
            value: machine ? bytes(machine.memory_used) : "—",
            detail: machine ? `of ${bytes(machine.memory_total)}` : "",
        },
        {
            label: "Uptime",
            value: data ? duration(data.uptime_seconds) : "—",
            detail: machine?.hostname ?? "",
        },
    ];
    return (
        <section aria-label="Monitoring" className="space-y-3">
            <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
                {metrics.map((metric) => (
                    <div key={metric.label} className="min-w-0 rounded-lg border bg-card px-5 py-4">
                        <p className="text-xs text-muted-foreground">{metric.label}</p>
                        <p className="mt-2 break-words text-2xl font-semibold tabular-nums tracking-tight">
                            {metric.value}
                        </p>
                        <p className="mt-1 truncate text-xs text-muted-foreground">
                            {metric.detail || "\u00a0"}
                        </p>
                    </div>
                ))}
            </div>
            {(status.error || system.error) && (
                <p role="alert" className="text-sm text-destructive">
                    Monitoring could not refresh. Displayed values may be out of date. Retrying…
                </p>
            )}
        </section>
    );
}
