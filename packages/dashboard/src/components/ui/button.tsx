import { Button as BaseButton } from "@base-ui/react/button";
import { HugeiconsIcon, type IconSvgElement } from "@hugeicons/react";
import * as stylex from "@stylexjs/stylex";
import type { ComponentProps } from "react";

import { colors, fontSizes, lineHeights, radii, space } from "../../tokens.stylex.ts";

const dark = "@media (prefers-color-scheme: dark)";
const ring = `color-mix(in oklab, ${colors.ring} 50%, transparent)`;
const shadow = "0 1px 2px 0 rgb(0 0 0 / 0.05)";

const styles = stylex.create({
  base: {
    display: "inline-flex",
    flexShrink: 0,
    alignItems: "center",
    justifyContent: "center",
    gap: space.s2,
    borderRadius: radii.md,
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
    fontWeight: 500,
    whiteSpace: "nowrap",
    transitionProperty: "all",
    transitionDuration: "150ms",
    transitionTimingFunction: "cubic-bezier(0.4, 0, 0.2, 1)",
    outlineStyle: "none",
    boxShadow: { default: null, ":focus-visible": `0 0 0 3px ${ring}` },
    pointerEvents: { default: null, ":disabled": "none" },
    opacity: { default: null, ":disabled": 0.5 },
  },
  icon: { flexShrink: 0, pointerEvents: "none", width: space.s4, height: space.s4 },
  iconXs: { width: space.s3, height: space.s3 },
});

const variants = stylex.create({
  default: {
    backgroundColor: {
      default: colors.primary,
      ":hover": `color-mix(in oklab, ${colors.primary} 90%, transparent)`,
    },
    color: colors.primaryForeground,
  },
  destructive: {
    backgroundColor: {
      default: colors.destructive,
      ":hover": `color-mix(in oklab, ${colors.destructive} 90%, transparent)`,
      [dark]: `color-mix(in oklab, ${colors.destructive} 60%, transparent)`,
    },
    color: "white",
  },
  outline: {
    borderWidth: "1px",
    borderStyle: "solid",
    borderColor: { default: colors.border, ":focus-visible": colors.ring, [dark]: colors.input },
    backgroundColor: {
      default: colors.background,
      ":hover": colors.accent,
      [dark]: {
        default: `color-mix(in oklab, ${colors.input} 30%, transparent)`,
        ":hover": `color-mix(in oklab, ${colors.input} 50%, transparent)`,
      },
    },
    color: { default: null, ":hover": colors.accentForeground },
    boxShadow: { default: shadow, ":focus-visible": `0 0 0 3px ${ring}, ${shadow}` },
  },
  ghost: {
    backgroundColor: {
      default: null,
      ":hover": colors.accent,
      [dark]: { default: null, ":hover": `color-mix(in oklab, ${colors.accent} 50%, transparent)` },
    },
    color: { default: null, ":hover": colors.accentForeground },
  },
});

const sizes = stylex.create({
  default: { minHeight: "2.75rem", height: "2.475rem", paddingBlock: space.s2, paddingInline: "1rem" },
  xs: {
    height: space.s6,
    gap: space.s1,
    paddingInline: space.s2,
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
  },
  sm: { height: space.s8, gap: space.s1_5, paddingInline: space.s3 },
  "icon-sm": { width: space.s8, height: space.s8 },
});

// Buttons with an icon and a label sit a little tighter.
const iconPadding = stylex.create({
  default: {},
  xs: { paddingInline: space.s1_5 },
  sm: { paddingInline: space.s2_5 },
  "icon-sm": {},
});

export function Button({
  variant = "default",
  size = "default",
  type = "button",
  icon,
  style,
  href,
  children,
  ...props
}: Omit<ComponentProps<"button">, "className" | "style"> & {
  variant?: keyof typeof variants;
  size?: keyof typeof sizes;
  icon?: IconSvgElement;
  style?: stylex.StyleXStyles;
  /** Renders a link to here that looks like the button. */
  href?: string;
}) {
  const element = href === undefined ? { type } : { render: <a href={href} />, nativeButton: false };
  return (
    <BaseButton
      {...element}
      {...props}
      {...stylex.props(
        styles.base,
        variants[variant],
        sizes[size],
        icon !== undefined && children !== undefined && iconPadding[size],
        style,
      )}
    >
      {icon && <HugeiconsIcon icon={icon} {...stylex.props(styles.icon, size === "xs" && styles.iconXs)} />}
      {children}
    </BaseButton>
  );
}
