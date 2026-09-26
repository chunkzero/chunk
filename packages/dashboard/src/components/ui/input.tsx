import type { ComponentProps } from "react";

import { cn } from "../../lib/cn.ts";

export const fieldClass =
  "w-full min-w-0 rounded-md border border-input bg-transparent px-3 py-1 text-base shadow-xs transition-[color,box-shadow] outline-none placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-destructive/20 md:text-sm dark:bg-input/30";

export function Input({ className, ...props }: ComponentProps<"input">) {
  return <input data-slot="input" className={cn("h-9", fieldClass, className)} {...props} />;
}
