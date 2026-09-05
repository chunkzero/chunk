import { useState } from "react";
import { Check, ChevronsUpDown, GitBranch, Loader2 } from "lucide-react";
import { useBranches } from "@/lib/api";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";

/// A combobox over the remote's branches that also accepts a typed branch name.
export function BranchPicker({
    id,
    repository,
    value,
    onChange,
    disabled,
}: {
    id?: string;
    repository: string | null;
    value: string;
    onChange: (branch: string) => void;
    disabled?: boolean;
}) {
    const [open, setOpen] = useState(false);
    const [query, setQuery] = useState("");
    const branches = useBranches(repository);
    const trimmed = query.trim();
    const names = branches.data?.branches ?? [];
    const matches = names.filter((name) => name.toLowerCase().includes(trimmed.toLowerCase()));
    const custom = trimmed && !names.includes(trimmed) ? trimmed : null;

    function select(branch: string) {
        onChange(branch);
        setQuery("");
        setOpen(false);
    }

    return (
        <Popover open={open} onOpenChange={setOpen}>
            <PopoverTrigger asChild>
                <button
                    id={id}
                    type="button"
                    role="combobox"
                    aria-expanded={open}
                    disabled={disabled}
                    className="flex h-9 w-full items-center gap-2 rounded-md border border-input bg-transparent px-3 text-sm shadow-xs outline-none focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50"
                >
                    <GitBranch className="size-4 shrink-0 text-muted-foreground" />
                    <span
                        className={cn(
                            "flex-1 truncate text-left font-mono text-xs",
                            !value && "font-sans text-sm text-muted-foreground",
                        )}
                    >
                        {value || "Select a branch"}
                    </span>
                    <ChevronsUpDown className="size-4 shrink-0 opacity-50" />
                </button>
            </PopoverTrigger>
            <PopoverContent className="w-(--radix-popover-trigger-width) p-0">
                <input
                    value={query}
                    onChange={(event) => setQuery(event.target.value)}
                    onKeyDown={(event) => {
                        if (event.key !== "Enter") return;
                        event.preventDefault();
                        const first = matches[0] ?? custom;
                        if (first) select(first);
                    }}
                    placeholder="Search or type a branch"
                    autoComplete="off"
                    spellCheck={false}
                    className="h-9 w-full border-b bg-transparent px-3 text-sm outline-none placeholder:text-muted-foreground"
                />
                <ul role="listbox" className="max-h-60 overflow-y-auto p-1">
                    {branches.isLoading && (
                        <Note>
                            <Loader2 className="size-3.5 animate-spin" /> Loading branches…
                        </Note>
                    )}
                    {branches.error && <Note>{branches.error.message}</Note>}
                    {matches.map((name) => (
                        <Option key={name} selected={name === value} onSelect={() => select(name)}>
                            <span className="truncate">{name}</span>
                            {name === branches.data?.default_branch && (
                                <span className="ml-auto font-sans text-muted-foreground">
                                    default
                                </span>
                            )}
                        </Option>
                    ))}
                    {custom && (
                        <Option selected={false} onSelect={() => select(custom)}>
                            <span className="font-sans">Use</span>
                            <span className="truncate">{custom}</span>
                        </Option>
                    )}
                    {branches.isSuccess && !matches.length && !custom && (
                        <Note>No branches match.</Note>
                    )}
                </ul>
            </PopoverContent>
        </Popover>
    );
}

function Option({
    selected,
    onSelect,
    children,
}: {
    selected: boolean;
    onSelect: () => void;
    children: React.ReactNode;
}) {
    return (
        <li role="option" aria-selected={selected}>
            <button
                type="button"
                onClick={onSelect}
                className="flex w-full items-center gap-2 rounded-sm px-2 py-1.5 text-left font-mono text-xs outline-none hover:bg-accent focus-visible:bg-accent"
            >
                <Check className={cn("size-3.5 shrink-0", !selected && "opacity-0")} />
                {children}
            </button>
        </li>
    );
}

function Note({ children }: { children: React.ReactNode }) {
    return (
        <li className="flex items-center gap-2 px-2 py-2 text-xs text-muted-foreground">
            {children}
        </li>
    );
}
