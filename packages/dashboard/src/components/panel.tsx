import * as stylex from "@stylexjs/stylex";
import type { ReactNode } from "react";

import { colors, fontSizes, lineHeights, radii, space } from "../tokens.stylex.ts";

const styles = stylex.create({
  panel: {
    borderWidth: "1px",
    borderStyle: "solid",
    borderColor: colors.border,
    borderRadius: radii.lg,
    backgroundColor: colors.card,
  },
  header: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: space.s4,
    minHeight: "3.3rem",
    paddingInline: space.s5,
    paddingBlock: space.s2,
    borderBottomWidth: "1px",
    borderBottomStyle: "solid",
    borderBottomColor: colors.border,
  },
  title: { fontSize: fontSizes.sm, lineHeight: lineHeights.sm, fontWeight: 500 },
  note: {
    paddingInline: space.s5,
    paddingBlock: space.s8,
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
    color: colors.mutedForeground,
  },
});

/** Rows of a divided list: a line between each row and the next. */
export const listStyles = stylex.create({
  row: {
    borderBottomWidth: { default: "1px", ":last-child": 0 },
    borderBottomStyle: "solid",
    borderBottomColor: colors.border,
  },
});

export function Panel({
  title,
  action,
  children,
  style,
}: {
  title?: string;
  action?: ReactNode;
  children: ReactNode;
  style?: stylex.StyleXStyles;
}) {
  return (
    <section {...stylex.props(styles.panel, style)}>
      {title && (
        <div {...stylex.props(styles.header)}>
          <h2 {...stylex.props(styles.title)}>{title}</h2>
          {action}
        </div>
      )}
      {children}
    </section>
  );
}

/** A panel's placeholder line, for empty and loading lists. */
export function PanelNote({ children }: { children: ReactNode }) {
  return <p {...stylex.props(styles.note)}>{children}</p>;
}
