import { Code, ConnectError } from "@connectrpc/connect";
import { Outlet, useParams } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { ErrorText, Page } from "../components/page.tsx";
import { Release } from "../components/release.tsx";
import { Status } from "../components/status.tsx";
import { errorMessage } from "../lib/client.ts";
import { environmentStatus, timeAgo } from "../lib/format.ts";
import { useEnvironment } from "../lib/queries.ts";
import { Missing } from "./missing.tsx";

export function Environment() {
  const { environment: environmentId } = useParams({ from: "/p/$project/$environment" });
  const { data: environment, error } = useEnvironment(environmentId);
  if (error && ConnectError.from(error).code === Code.NotFound) return <Missing />;

  return (
    <Page>
      <div className="space-y-3">
        <div className="flex items-center gap-3">
          <h1 className="text-xl font-semibold tracking-tight">{environment?.name ?? "…"}</h1>
          {environment && <Status status={environmentStatus[environment.state]} />}
        </div>
        {environment && (
          <dl className="flex flex-wrap gap-x-8 gap-y-2 text-sm">
            <Fact label="Hostname">
              <span className="font-mono text-xs">{environment.hostname || "None"}</span>
            </Fact>
            <Fact label="Players">{environment.onlinePlayers}</Fact>
            <Fact label="Release">
              <Release deploymentId={environment.activeDeploymentId} />
            </Fact>
            <Fact label="Created">{timeAgo(environment.createTime)}</Fact>
          </dl>
        )}
      </div>
      <ErrorText error={error ? errorMessage(error) : undefined} />
      <Outlet />
    </Page>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex items-baseline gap-2">
      <dt className="text-muted-foreground">{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}
