import type { ReactNode } from "react";

export function Panel({
    title,
    action,
    children,
    className = "",
}: {
    title?: string;
    action?: ReactNode;
    children: ReactNode;
    className?: string;
}) {
    return (
        <section className={`rounded-lg border bg-card ${className}`}>
            {title && (
                <div className="flex items-center justify-between border-b px-5 py-3">
                    <h2 className="text-sm font-medium">{title}</h2>
                    {action}
                </div>
            )}
            {children}
        </section>
    );
}

export function Rows({ children }: { children: ReactNode }) {
    return <dl className="divide-y text-sm">{children}</dl>;
}

export function Row({ label, children }: { label: string; children: ReactNode }) {
    return (
        <div className="flex items-center justify-between gap-4 px-5 py-3">
            <dt className="text-muted-foreground">{label}</dt>
            <dd className="text-right font-mono text-xs">{children}</dd>
        </div>
    );
}
