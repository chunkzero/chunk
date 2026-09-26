import { ArrowRight01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { createLink } from "@tanstack/react-router";
import type { ComponentProps } from "react";

/** A full-width row link in a divided list, ending in a chevron. */
export const ListLink = createLink(({ children, ...props }: ComponentProps<"a">) => (
  <a
    {...props}
    className="flex items-center gap-4 px-5 py-4 transition-colors hover:bg-accent/60 focus-visible:bg-accent/60 focus-visible:outline-none"
  >
    {children}
    <HugeiconsIcon icon={ArrowRight01Icon} className="size-4 shrink-0 text-muted-foreground/60" />
  </a>
));
