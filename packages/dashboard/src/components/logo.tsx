import * as stylex from "@stylexjs/stylex";

import { colors, space } from "../tokens.stylex.ts";

const styles = stylex.create({
  logo: { display: "flex", alignItems: "center", gap: space.s2, fontWeight: 600 },
  mark: { width: space.s4, height: space.s4, borderRadius: "3px", backgroundColor: colors.foreground },
});

export function Logo() {
  return (
    <span {...stylex.props(styles.logo)}>
      <span {...stylex.props(styles.mark)} />
      chunk
    </span>
  );
}
