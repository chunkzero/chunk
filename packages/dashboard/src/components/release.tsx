import { useDeployment } from "../lib/queries.ts";

/** The release a deployment serves, by deployment ID. */
export function Release({ deploymentId }: { deploymentId: string }) {
  const deployment = useDeployment(deploymentId);
  if (!deploymentId) return <span className="text-muted-foreground">Nothing deployed</span>;
  return <ReleaseId id={deployment.data?.releaseId ?? "…"} />;
}

export function ReleaseId({ id }: { id: string }) {
  return (
    <span title={id} className="inline-block max-w-40 truncate align-bottom font-mono text-xs">
      {id}
    </span>
  );
}
