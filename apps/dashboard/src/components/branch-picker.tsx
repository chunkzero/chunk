import { useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
    Tick02Icon,
    UnfoldMoreIcon,
    GitBranchIcon,
    Loading03Icon,
} from "@hugeicons/core-free-icons";
import { useBranches } from "@/lib/api";
import { Button } from "@/components/ui/button";
import {
    Command,
    CommandGroup,
    CommandInput,
    CommandItem,
    CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";

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
        <Popover
            modal
            open={open}
            onOpenChange={(next) => {
                setOpen(next);
                if (!next) setQuery("");
            }}
        >
            <PopoverTrigger asChild>
                <Button
                    id={id}
                    type="button"
                    variant="outline"
                    role="combobox"
                    aria-expanded={open}
                    disabled={disabled}
                    className="w-full justify-start font-normal"
                >
                    <HugeiconsIcon
                        icon={GitBranchIcon}
                        className="size-4 shrink-0 text-muted-foreground"
                    />
                    <span
                        className={cn(
                            "flex-1 truncate text-left font-mono text-xs",
                            !value && "font-sans text-sm text-muted-foreground",
                        )}
                    >
                        {value || "Select a branch"}
                    </span>
                    <HugeiconsIcon icon={UnfoldMoreIcon} className="size-4 shrink-0 opacity-50" />
                </Button>
            </PopoverTrigger>
            <PopoverContent
                align="start"
                collisionPadding={12}
                className="w-(--radix-popover-trigger-width) max-h-(--radix-popover-content-available-height) overflow-hidden p-0"
            >
                <Command shouldFilter={false}>
                    <CommandInput
                        value={query}
                        onValueChange={setQuery}
                        placeholder="Search or type a branch"
                        aria-label="Search branches"
                        autoComplete="off"
                        spellCheck={false}
                    />
                    <CommandList className="max-h-[min(15rem,calc(var(--radix-popover-content-available-height)-3rem))] overscroll-contain">
                        {branches.isLoading && (
                            <Note>
                                <HugeiconsIcon
                                    icon={Loading03Icon}
                                    className="size-3.5 animate-spin"
                                />{" "}
                                Loading branches…
                            </Note>
                        )}
                        {branches.error && <Note>{branches.error.message}</Note>}
                        <CommandGroup>
                            {matches.map((name) => (
                                <CommandItem
                                    key={name}
                                    value={name}
                                    onSelect={() => select(name)}
                                    className="font-mono text-xs"
                                >
                                    <HugeiconsIcon
                                        icon={Tick02Icon}
                                        className={cn(
                                            "size-3.5 shrink-0",
                                            name !== value && "opacity-0",
                                        )}
                                    />
                                    <span className="truncate">{name}</span>
                                    {name === branches.data?.default_branch && (
                                        <span className="ml-auto font-sans text-muted-foreground">
                                            default
                                        </span>
                                    )}
                                </CommandItem>
                            ))}
                            {custom && (
                                <CommandItem
                                    value={custom}
                                    onSelect={() => select(custom)}
                                    className="font-mono text-xs"
                                >
                                    <span className="font-sans">Use</span>
                                    <span className="truncate">{custom}</span>
                                </CommandItem>
                            )}
                        </CommandGroup>
                        {branches.isSuccess && !matches.length && !custom && (
                            <Note>No branches match.</Note>
                        )}
                    </CommandList>
                </Command>
            </PopoverContent>
        </Popover>
    );
}

function Note({ children }: { children: React.ReactNode }) {
    return (
        <div
            role="status"
            className="flex items-center gap-2 px-3 py-2 text-xs text-muted-foreground"
        >
            {children}
        </div>
    );
}
