import { useState } from "react";
import {
    type Deployment,
    type Project,
    useAddTarget,
    useEditable,
    useRemoveTarget,
    useUpdateTarget,
} from "@/lib/api";
import { environmentLabels, environmentOrder } from "@/lib/format";
import { BranchPicker } from "@/components/branch-picker";
import { Button } from "@/components/ui/button";
import {
    Dialog,
    DialogClose,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
    SelectValue,
} from "@/components/ui/select";

/// Creates a target, or edits `target` when one is given.
export function TargetDialog({
    project,
    target,
    initialBranch,
    open,
    onOpenChange,
}: {
    project: Project;
    target?: Deployment;
    initialBranch?: string;
    open: boolean;
    onOpenChange: (open: boolean) => void;
}) {
    return (
        <Dialog open={open} onOpenChange={onOpenChange}>
            <DialogContent>
                <TargetForm
                    project={project}
                    target={target}
                    initialBranch={initialBranch}
                    close={() => onOpenChange(false)}
                />
            </DialogContent>
        </Dialog>
    );
}

function TargetForm({
    project,
    target,
    initialBranch,
    close,
}: {
    project: Project;
    target?: Deployment;
    initialBranch?: string;
    close: () => void;
}) {
    const add = useAddTarget(project.id);
    const update = useUpdateTarget(project.id);
    const { editable, reason } = useEditable();
    const [branch, setBranch] = useState(target?.git_ref ?? initialBranch ?? "");
    const [environment, setEnvironment] = useState<Deployment["environment"]>(
        target?.environment ?? "development",
    );
    const [name, setName] = useState(target?.name ?? "");
    const pending = add.isPending || update.isPending;
    const error = add.error ?? update.error;
    const disabled = !editable || pending;

    function reset() {
        add.reset();
        update.reset();
    }

    return (
        <form
            className="contents"
            onSubmit={(event) => {
                event.preventDefault();
                const spec = { branch: branch.trim(), environment, name: name.trim() || null };
                if (target) update.mutate({ target: target.id, spec }, { onSuccess: close });
                else add.mutate(spec, { onSuccess: close });
            }}
        >
            <DialogHeader>
                <DialogTitle>{target ? "Edit target" : "Add deployment target"}</DialogTitle>
                <DialogDescription>
                    A target records which branch of {project.name} deploys to an environment.
                    Automatic builds are not available yet.
                </DialogDescription>
            </DialogHeader>
            <fieldset disabled={disabled} className="space-y-4 disabled:opacity-60">
                <div className="space-y-2">
                    <Label htmlFor="target-branch">Branch</Label>
                    <BranchPicker
                        id="target-branch"
                        repository={project.source?.repository ?? null}
                        value={branch}
                        onChange={(value) => {
                            setBranch(value);
                            reset();
                        }}
                        disabled={disabled}
                    />
                </div>
                <div className="space-y-2">
                    <Label htmlFor="target-environment">Environment</Label>
                    <Select
                        value={environment}
                        onValueChange={(value) => {
                            const selected = environmentOrder.find((entry) => entry === value);
                            if (selected) {
                                setEnvironment(selected);
                                reset();
                            }
                        }}
                    >
                        <SelectTrigger id="target-environment" className="w-full">
                            <SelectValue />
                        </SelectTrigger>
                        <SelectContent position="popper">
                            {environmentOrder.map((value) => (
                                <SelectItem key={value} value={value}>
                                    {environmentLabels[value]}
                                </SelectItem>
                            ))}
                        </SelectContent>
                    </Select>
                </div>
                <div className="space-y-2">
                    <Label htmlFor="target-name">
                        Name <span className="font-normal text-muted-foreground">(optional)</span>
                    </Label>
                    <Input
                        id="target-name"
                        value={name}
                        onChange={(event) => {
                            setName(event.target.value);
                            reset();
                        }}
                        placeholder={branch || "Same as the branch"}
                        maxLength={100}
                        autoComplete="off"
                    />
                </div>
            </fieldset>
            {error && (
                <p role="alert" className="text-sm text-destructive">
                    {error.message}
                </p>
            )}
            {reason && <p className="text-xs text-muted-foreground">{reason}</p>}
            <DialogFooter>
                <DialogClose asChild>
                    <Button type="button" variant="outline" size="sm">
                        Cancel
                    </Button>
                </DialogClose>
                <Button type="submit" size="sm" disabled={disabled || !branch.trim()}>
                    {pending ? "Saving…" : target ? "Save" : "Add target"}
                </Button>
            </DialogFooter>
        </form>
    );
}

export function RemoveTargetDialog({
    project,
    target,
    open,
    onOpenChange,
    onRemoved,
}: {
    project: Project;
    target: Deployment;
    open: boolean;
    onOpenChange: (open: boolean) => void;
    onRemoved?: () => void;
}) {
    const mutation = useRemoveTarget(project.id);
    return (
        <Dialog open={open} onOpenChange={onOpenChange}>
            <DialogContent>
                <DialogHeader>
                    <DialogTitle>Remove {target.name}?</DialogTitle>
                    <DialogDescription>
                        This removes the target from {project.name}. Nothing is deployed or deleted.
                    </DialogDescription>
                </DialogHeader>
                {mutation.error && (
                    <p role="alert" className="text-sm text-destructive">
                        {mutation.error.message}
                    </p>
                )}
                <DialogFooter>
                    <DialogClose asChild>
                        <Button type="button" variant="outline" size="sm">
                            Cancel
                        </Button>
                    </DialogClose>
                    <Button
                        variant="destructive"
                        size="sm"
                        disabled={mutation.isPending}
                        onClick={() =>
                            mutation.mutate(target.id, {
                                onSuccess: () => {
                                    onOpenChange(false);
                                    onRemoved?.();
                                },
                            })
                        }
                    >
                        {mutation.isPending ? "Removing…" : "Remove target"}
                    </Button>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    );
}
