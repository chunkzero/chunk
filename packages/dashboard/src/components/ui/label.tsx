import * as stylex from "@stylexjs/stylex";
import type { ComponentProps } from "react";

import { fontSizes, space } from "../../tokens.stylex.ts";

const styles = stylex.create({
  label: {
    display: "flex",
    alignItems: "center",
    gap: space.s2,
    fontSize: fontSizes.sm,
    lineHeight: 1,
    fontWeight: 500,
    userSelect: "none",
  },
});

export function Label(props: Omit<ComponentProps<"label">, "className" | "style">) {
  return <label {...props} {...stylex.props(styles.label)} />;
}
