import * as stylex from "@stylexjs/stylex";

import type { Status as StatusData } from "../lib/format.ts";
import { colors, fontSizes, lineHeights, space } from "../tokens.stylex.ts";

const styles = stylex.create({
  status: {
    display: "inline-flex",
    alignItems: "center",
    gap: space.s1_5,
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
    color: colors.mutedForeground,
  },
  dot: { display: "inline-block", flexShrink: 0, width: space.s2, height: space.s2, borderRadius: "9999px" },
});

const tones = stylex.create({
  success: { backgroundColor: colors.success },
  warning: { backgroundColor: colors.warning },
  danger: { backgroundColor: colors.destructive },
  muted: { backgroundColor: `color-mix(in oklab, ${colors.mutedForeground} 50%, transparent)` },
});

export function Status({ status }: { status: StatusData }) {
  return (
    <span {...stylex.props(styles.status)}>
      <span {...stylex.props(styles.dot, tones[status.tone])} />
      {status.label}
    </span>
  );
}
