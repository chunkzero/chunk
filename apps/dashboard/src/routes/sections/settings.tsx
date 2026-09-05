import { useState } from "react";
import { Link, useNavigate } from "@tanstack/react-router";
import { Pencil } from "lucide-react";
import { Panel, Row, Rows } from "@/components/panel";
import { RemoveTargetDialog, TargetDialog } from "@/components/target-dialog";
import { Button } from "@/components/ui/button";
import { useEditable } from "@/lib/api";
import { environmentLabels, repoLabel, timeAgo } from "@/lib/format";
import { useDeployment } from "@/routes/deployment";

export function Settings() {
    const { project, deployment } = useDeployment();
    const { editable, reason } = useEditable();
    const navigate = useNavigate();
    const [dialog, setDialog] = useState<"edit" | "remove" | null>(null);
    if (!project || !deployment) return null;
    const close = (open: boolean) => {
        if (!open) setDialog(null);
    };
    return (
        <div className="space-y-6">
            <h1 className="text-xl font-semibold">Settings</h1>
            <Panel
                title="Target"
                action={
                    <Button
                        size="sm"
                        variant="ghost"
                        disabled={!editable}
                        title={reason}
                        onClick={() => setDialog("edit")}
                    >
                        <Pencil /> Edit
                    </Button>
                }
            >
                <Rows>
                    <Row label="Name">
                        <span className="break-all">{deployment.name}</span>
                    </Row>
                    <Row label="Branch">
                        <span className="break-all">{deployment.git_ref ?? "Not configured"}</span>
                    </Row>
                    <Row label="Environment">{environmentLabels[deployment.environment]}</Row>
                    <Row label="Last deployed">
                        {deployment.deployed_at ? timeAgo(deployment.deployed_at) : "Never"}
                    </Row>
                </Rows>
            </Panel>
            <Panel
                title="Repository"
                action={
                    <Button asChild size="sm" variant="ghost">
                        <Link to="/p/$project" params={{ project: project.id }}>
                            Manage in {project.name}
                        </Link>
                    </Button>
                }
            >
                {project.source ? (
                    <Rows>
                        <Row label="Repository">
                            <a
                                href={project.source.repository}
                                target="_blank"
                                rel="noreferrer"
                                className="break-all hover:underline"
                            >
                                {repoLabel(project.source.repository)}
                            </a>
                        </Row>
                        <Row label="Default branch">{project.source.branch ?? "Not set"}</Row>
                    </Rows>
                ) : (
                    <p className="px-5 py-4 text-sm text-muted-foreground">
                        No repository is linked to this application.
                    </p>
                )}
            </Panel>
            <Panel title="Danger zone" className="border-destructive/40">
                <div className="flex items-center justify-between gap-4 px-5 py-4">
                    <div className="space-y-1">
                        <p className="text-sm font-medium">Remove this target</p>
                        <p className="text-xs text-muted-foreground">
                            Removes {deployment.name} from {project.name}. Nothing is deployed or
                            deleted.
                        </p>
                    </div>
                    <Button
                        variant="destructive"
                        size="sm"
                        disabled={!editable}
                        title={reason}
                        onClick={() => setDialog("remove")}
                    >
                        Remove
                    </Button>
                </div>
            </Panel>
            <TargetDialog
                project={project}
                target={deployment}
                open={dialog === "edit"}
                onOpenChange={close}
            />
            <RemoveTargetDialog
                project={project}
                target={deployment}
                open={dialog === "remove"}
                onOpenChange={close}
                onRemoved={() =>
                    void navigate({ to: "/p/$project", params: { project: project.id } })
                }
            />
        </div>
    );
}
