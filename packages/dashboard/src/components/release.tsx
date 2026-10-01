import * as stylex from "@stylexjs/stylex";

import { useDeployment } from "../lib/queries.ts";
import { colors, fonts, fontSizes, lineHeights } from "../tokens.stylex.ts";

const styles = stylex.create({
  none: { color: colors.mutedForeground },
  id: {
    display: "inline-block",
    maxWidth: "11rem",
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
    verticalAlign: "bottom",
    fontFamily: fonts.mono,
    fontSize: fontSizes.xs,
    lineHeight: lineHeights.xs,
  },
});

/** The release a deployment serves, by deployment ID. */
export function Release({ deploymentId }: { deploymentId: string }) {
  const deployment = useDeployment(deploymentId);
  if (!deploymentId) return <span {...stylex.props(styles.none)}>Nothing deployed</span>;
  return <ReleaseId id={deployment.data?.releaseId ?? "…"} />;
}

export function ReleaseId({ id }: { id: string }) {
  return (
    <span title={id} {...stylex.props(styles.id)}>
      {id}
    </span>
  );
}
