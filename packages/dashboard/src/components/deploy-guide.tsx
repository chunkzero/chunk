import * as stylex from "@stylexjs/stylex";

import { colors } from "../tokens.stylex.ts";

const styles = stylex.create({
  link: {
    color: colors.link,
    textUnderlineOffset: "4px",
    textDecorationLine: { default: null, ":hover": "underline" },
  },
});

export function DeployGuideLink() {
  return (
    <a
      href="https://github.com/chunkzero/chunk/blob/main/deploy/compose/README.md#deploying"
      {...stylex.props(styles.link)}
    >
      self-hosting guide
    </a>
  );
}
