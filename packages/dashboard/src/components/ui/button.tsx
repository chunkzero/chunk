import type { ComponentProps } from "react";

import { cn } from "../../lib/cn.ts";

const variants = {
  default: "bg-primary text-primary-foreground hover:bg-primary/90",
  destructive: "bg-destructive text-white hover:bg-destructive/90 dark:bg-destructive/60",
  outline:
    "border bg-background shadow-xs hover:bg-accent hover:text-accent-foreground dark:border-input dark:bg-input/30 dark:hover:bg-input/50",
  ghost: "hover:bg-accent hover:text-accent-foreground dark:hover:bg-accent/50",
};

const sizes = {
  default: "h-9 px-4 py-2 has-[>svg]:px-3",
  xs: "h-6 gap-1 px-2 text-xs has-[>svg]:px-1.5 [&_svg:not([class*='size-'])]:size-3",
  sm: "h-8 gap-1.5 px-3 has-[>svg]:px-2.5",
  "icon-sm": "size-8",
};

export function Button({
  className,
  variant = "default",
  size = "default",
  type = "button",
  ...props
}: ComponentProps<"button"> & { variant?: keyof typeof variants; size?: keyof typeof sizes }) {
  return (
    <button
      type={type}
      data-slot="button"
      data-size={size}
      className={cn(
        "inline-flex shrink-0 items-center justify-center gap-2 rounded-md text-sm font-medium whitespace-nowrap transition-all outline-none focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
        variants[variant],
        sizes[size],
        className,
      )}
      {...props}
    />
  );
}
