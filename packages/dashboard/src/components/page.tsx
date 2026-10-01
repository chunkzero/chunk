import * as stylex from "@stylexjs/stylex";
import type { ReactNode } from "react";

import { colors, fontSizes, lineHeights, radii, space } from "../tokens.stylex.ts";

const fadeIn = stylex.keyframes({ from: { opacity: 0 } });

const styles = stylex.create({
  page: {
    flex: 1,
    display: "flex",
    flexDirection: "column",
    gap: space.s6,
    width: "100%",
    maxWidth: "72rem",
    marginInline: "auto",
    paddingInline: space.s6,
    paddingBlock: space.s8,
    animationName: fadeIn,
    animationDuration: "200ms",
  },
  empty: {
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    minHeight: "13.2rem",
    padding: space.s8,
    borderWidth: "1px",
    borderStyle: "dashed",
    borderColor: colors.border,
    borderRadius: radii.lg,
    textAlign: "center",
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
    color: colors.mutedForeground,
  },
  error: { fontSize: fontSizes.sm, lineHeight: lineHeights.sm, color: colors.destructive },
});

export function Page({ children }: { children: ReactNode }) {
  return <main {...stylex.props(styles.page)}>{children}</main>;
}

export function EmptyState({ children }: { children: ReactNode }) {
  return <div {...stylex.props(styles.empty)}>{children}</div>;
}

export function ErrorText({ error, style }: { error: string | undefined; style?: stylex.StyleXStyles }) {
  if (!error) return null;
  return (
    <p role="alert" {...stylex.props(styles.error, style)}>
      {error}
    </p>
  );
}
