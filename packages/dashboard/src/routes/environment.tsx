import { Code, ConnectError } from "@connectrpc/connect";
import * as stylex from "@stylexjs/stylex";
import { Outlet, useParams } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { ErrorText, Page } from "../components/page.tsx";
import { Release } from "../components/release.tsx";
import { Status } from "../components/status.tsx";
import { errorMessage } from "../lib/client.ts";
import { environmentStatus, timeAgo } from "../lib/format.ts";
import { useEnvironment } from "../lib/queries.ts";
import { colors, fonts, fontSizes, lineHeights, space } from "../tokens.stylex.ts";
import { Missing } from "./missing.tsx";

const styles = stylex.create({
  summary: { display: "flex", flexDirection: "column", gap: space.s3 },
  heading: { display: "flex", alignItems: "center", gap: space.s3 },
  title: { fontSize: fontSizes.xl, lineHeight: lineHeights.xl, fontWeight: 600, letterSpacing: "-0.025em" },
  facts: {
    display: "flex",
    flexWrap: "wrap",
    columnGap: space.s8,
    rowGap: space.s2,
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
  },
  mono: { fontFamily: fonts.mono, fontSize: fontSizes.xs, lineHeight: lineHeights.xs },
  fact: { display: "flex", alignItems: "baseline", gap: space.s2 },
  label: { color: colors.mutedForeground },
});

export function Environment() {
  const { environment: environmentId } = useParams({ from: "/p/$project/$environment" });
  const { data: environment, error } = useEnvironment(environmentId);
  if (error && ConnectError.from(error).code === Code.NotFound) return <Missing />;

  return (
    <Page>
      <div {...stylex.props(styles.summary)}>
        <div {...stylex.props(styles.heading)}>
          <h1 {...stylex.props(styles.title)}>{environment?.name ?? "…"}</h1>
          {environment && <Status status={environmentStatus[environment.state]} />}
        </div>
        {environment && (
          <dl {...stylex.props(styles.facts)}>
            <Fact label="Join address">
              <span {...stylex.props(styles.mono)}>{environment.joinAddress || "None"}</span>
            </Fact>
            <Fact label="Players">{environment.onlinePlayers}</Fact>
            <Fact label="Release">
              <Release deploymentId={environment.activeDeploymentId} />
            </Fact>
            {environment.forkedFromEnvironmentId && (
              <Fact label="Forked from">
                <span {...stylex.props(styles.mono)}>{environment.forkedFromEnvironmentId}</span>
              </Fact>
            )}
            <Fact label="Created">{timeAgo(environment.createTime)}</Fact>
          </dl>
        )}
      </div>
      <ErrorText error={error ? errorMessage(error) : undefined} />
      <Outlet />
    </Page>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div {...stylex.props(styles.fact)}>
      <dt {...stylex.props(styles.label)}>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}
