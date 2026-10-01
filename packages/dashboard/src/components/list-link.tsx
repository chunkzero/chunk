import { ArrowRight01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import * as stylex from "@stylexjs/stylex";
import { createLink } from "@tanstack/react-router";
import type { ComponentProps } from "react";

import { colors, space } from "../tokens.stylex.ts";

const highlight = `color-mix(in oklab, ${colors.accent} 60%, transparent)`;

const styles = stylex.create({
  link: {
    display: "flex",
    alignItems: "center",
    gap: space.s4,
    paddingInline: space.s5,
    paddingBlock: space.s4,
    transitionProperty: "color, background-color, border-color, outline-color, text-decoration-color, fill, stroke",
    transitionDuration: "150ms",
    transitionTimingFunction: "cubic-bezier(0.4, 0, 0.2, 1)",
    backgroundColor: { default: null, ":hover": highlight, ":focus-visible": highlight },
    outlineStyle: { default: null, ":focus-visible": "none" },
  },
  chevron: {
    flexShrink: 0,
    width: space.s4,
    height: space.s4,
    color: `color-mix(in oklab, ${colors.mutedForeground} 60%, transparent)`,
  },
});

/** A full-width row link in a divided list, ending in a chevron. */
export const ListLink = createLink(({ children, ...props }: ComponentProps<"a">) => (
  <a {...props} {...stylex.props(styles.link)}>
    {children}
    <HugeiconsIcon icon={ArrowRight01Icon} {...stylex.props(styles.chevron)} />
  </a>
));
