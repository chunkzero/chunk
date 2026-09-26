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
        <div className="flex min-h-12 items-center justify-between gap-4 border-b px-5 py-2">
          <h2 className="text-sm font-medium">{title}</h2>
          {action}
        </div>
      )}
      {children}
    </section>
  );
}

/** A panel's placeholder line, for empty and loading lists. */
export function PanelNote({ children }: { children: ReactNode }) {
  return <p className="px-5 py-8 text-sm text-muted-foreground">{children}</p>;
}
