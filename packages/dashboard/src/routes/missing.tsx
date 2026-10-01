import * as stylex from "@stylexjs/stylex";
import { Link } from "@tanstack/react-router";

import { Page } from "../components/page.tsx";
import { colors, fontSizes, lineHeights } from "../tokens.stylex.ts";

const styles = stylex.create({
  text: { fontSize: fontSizes.sm, lineHeight: lineHeights.sm, color: colors.mutedForeground },
  link: {
    color: colors.link,
    textUnderlineOffset: "4px",
    textDecorationLine: { default: null, ":hover": "underline" },
  },
});

export function Missing() {
  return (
    <Page>
      <p {...stylex.props(styles.text)}>
        Nothing here.{" "}
        <Link to="/" {...stylex.props(styles.link)}>
          Back to projects
        </Link>
      </p>
    </Page>
  );
}
