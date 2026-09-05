import { useEffect, useState } from "react";
import { type Project, useBranches, useEditable, useUpdateSource } from "@/lib/api";
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

export function SourceDialog({
    project,
    open,
    onOpenChange,
}: {
    project: Project;
    open: boolean;
    onOpenChange: (open: boolean) => void;
}) {
    return (
        <Dialog open={open} onOpenChange={onOpenChange}>
            <DialogContent>
                <SourceForm project={project} close={() => onOpenChange(false)} />
            </DialogContent>
        </Dialog>
    );
}

function SourceForm({ project, close }: { project: Project; close: () => void }) {
    const mutation = useUpdateSource(project.id);
    const { editable, reason } = useEditable();
    const [repository, setRepository] = useState(project.source?.repository ?? "");
    // `null` means "use whatever the remote says its default branch is".
    const [branch, setBranch] = useState<string | null>(project.source?.branch ?? null);
    const lookup = useDebounced(repository.trim(), 500);
    const branches = useBranches(lookup || null);
    const selected = branch ?? branches.data?.default_branch ?? "";
    const disabled = !editable || mutation.isPending;

    return (
        <form
            className="contents"
            onSubmit={(event) => {
                event.preventDefault();
                mutation.mutate(
                    { repository: repository.trim(), branch: selected || null },
                    { onSuccess: close },
                );
            }}
        >
            <DialogHeader>
                <DialogTitle>
                    {project.source ? "Edit repository" : "Link a repository"}
                </DialogTitle>
                <DialogDescription>
                    Deployment targets for {project.name} are branches of this repository.
                </DialogDescription>
            </DialogHeader>
            <fieldset disabled={disabled} className="space-y-4 disabled:opacity-60">
                <div className="space-y-2">
                    <Label htmlFor="source-repository">Repository URL</Label>
                    <Input
                        id="source-repository"
                        value={repository}
                        onChange={(event) => {
                            setRepository(event.target.value);
                            setBranch(null);
                            mutation.reset();
                        }}
                        placeholder="https://github.com/owner/repository"
                        required
                        maxLength={2048}
                        autoComplete="off"
                        spellCheck={false}
                    />
                </div>
                <div className="space-y-2">
                    <Label htmlFor="source-branch">Default branch</Label>
                    <BranchPicker
                        id="source-branch"
                        repository={lookup || null}
                        value={selected}
                        onChange={(value) => {
                            setBranch(value);
                            mutation.reset();
                        }}
                        disabled={disabled}
                    />
                    <p className="text-xs text-muted-foreground">
                        {branches.isLoading
                            ? "Checking the repository…"
                            : branches.error
                              ? "Branches could not be loaded. Type a branch name instead."
                              : branches.data
                                ? `${branches.data.branches.length} branches found.`
                                : "Branches load once a repository URL is entered."}
                    </p>
                </div>
            </fieldset>
            {mutation.error && (
                <p role="alert" className="text-sm text-destructive">
                    {mutation.error.message}
                </p>
            )}
            {reason && <p className="text-xs text-muted-foreground">{reason}</p>}
            <DialogFooter>
                <DialogClose asChild>
                    <Button type="button" variant="outline" size="sm">
                        Cancel
                    </Button>
                </DialogClose>
                <Button type="submit" size="sm" disabled={disabled || !repository.trim()}>
                    {mutation.isPending ? "Saving…" : "Save"}
                </Button>
            </DialogFooter>
        </form>
    );
}

function useDebounced(value: string, delay: number) {
    const [debounced, setDebounced] = useState(value);
    useEffect(() => {
        const handle = setTimeout(() => setDebounced(value), delay);
        return () => clearTimeout(handle);
    }, [value, delay]);
    return debounced;
}
