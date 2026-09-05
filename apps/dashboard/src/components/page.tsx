import type { ReactNode } from "react";

export function Page({ children }: { children: ReactNode }) {
    return <main className="mx-auto w-full max-w-6xl flex-1 space-y-6 px-6 py-8">{children}</main>;
}

export function EmptyState({ children }: { children: ReactNode }) {
    return (
        <div className="flex min-h-64 items-center justify-center rounded-lg border border-dashed p-8 text-center text-sm text-muted-foreground">
            {children}
        </div>
    );
}
