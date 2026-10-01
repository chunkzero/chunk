import { ArrowRight01Icon, Logout01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import * as stylex from "@stylexjs/stylex";
import { Link, useParams } from "@tanstack/react-router";

import { signOut } from "../lib/client.ts";
import { useEnvironment, usePrincipal, useProject } from "../lib/queries.ts";
import { colors, fontSizes, lineHeights, space } from "../tokens.stylex.ts";
import { Logo } from "./logo.tsx";
import { Button } from "./ui/button.tsx";

const sections = [
  { to: "/p/$project/$environment", title: "Deployments" },
  { to: "/p/$project/$environment/logs", title: "Logs" },
  { to: "/p/$project/$environment/secrets", title: "Secrets" },
  { to: "/p/$project/$environment/domains", title: "Domains" },
] as const;

const sm = "@media (min-width: 40rem)";

const styles = stylex.create({
  header: {
    borderBottomWidth: "1px",
    borderBottomStyle: "solid",
    borderBottomColor: colors.border,
    backgroundColor: colors.card,
  },
  bar: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: space.s4,
    height: "3.575rem",
    maxWidth: "72rem",
    marginInline: "auto",
    paddingInline: space.s6,
  },
  breadcrumb: {
    display: "flex",
    alignItems: "center",
    gap: space.s2,
    minWidth: 0,
    overflowX: "auto",
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
    whiteSpace: "nowrap",
  },
  crumb: { flexShrink: 0 },
  current: { fontWeight: 500 },
  separator: {
    width: space.s4,
    height: space.s4,
    color: `color-mix(in oklab, ${colors.mutedForeground} 60%, transparent)`,
  },
  wide: { display: { default: "none", [sm]: "inline" }, color: colors.mutedForeground },
  account: { display: "flex", flexShrink: 0, alignItems: "center", gap: space.s2 },
  name: { fontSize: fontSizes.sm, lineHeight: lineHeights.sm },
  signOut: { color: { default: colors.mutedForeground, ":hover": colors.accentForeground } },
  tabs: {
    display: "flex",
    gap: space.s1,
    maxWidth: "72rem",
    marginInline: "auto",
    overflowX: "auto",
    paddingInline: space.s4,
    fontSize: fontSizes.sm,
    lineHeight: lineHeights.sm,
  },
  tab: { position: "relative", paddingInline: space.s2, paddingBlock: space.s2_5 },
  active: {
    "::after": {
      content: "''",
      position: "absolute",
      insetInline: 0,
      bottom: "-1px",
      height: "0.1375rem",
      backgroundColor: colors.foreground,
    },
  },
  inactive: { color: { default: colors.mutedForeground, ":hover": colors.foreground } },
});

export function TopNav() {
  const params = useParams({ strict: false });
  const principal = usePrincipal();

  return (
    <header {...stylex.props(styles.header)}>
      <div {...stylex.props(styles.bar)}>
        <nav aria-label="Breadcrumb" {...stylex.props(styles.breadcrumb)}>
          <Link to="/" {...stylex.props(styles.crumb)}>
            <Logo />
          </Link>
          <span {...stylex.props(styles.crumb, styles.wide)}>{window.location.hostname}</span>
          {params.project && <ProjectCrumb project={params.project} />}
          {params.project && params.environment && (
            <EnvironmentCrumb project={params.project} environment={params.environment} />
          )}
        </nav>
        <div {...stylex.props(styles.account)}>
          <span {...stylex.props(styles.name, styles.wide)}>{principal.data?.principal?.displayName}</span>
          <Button variant="ghost" size="sm" icon={Logout01Icon} style={styles.signOut} onClick={signOut}>
            Sign out
          </Button>
        </div>
      </div>
      {params.project && params.environment && <Tabs project={params.project} environment={params.environment} />}
    </header>
  );
}

function ProjectCrumb({ project }: { project: string }) {
  const { data } = useProject(project);
  return (
    <>
      <Crumb />
      <Link to="/p/$project" params={{ project }} {...stylex.props(styles.crumb, styles.current)}>
        {data?.name ?? "…"}
      </Link>
    </>
  );
}

function EnvironmentCrumb({ project, environment }: { project: string; environment: string }) {
  const { data } = useEnvironment(environment);
  return (
    <>
      <Crumb />
      <Link
        to="/p/$project/$environment"
        params={{ project, environment }}
        {...stylex.props(styles.crumb, styles.current)}
      >
        {data?.name ?? "…"}
      </Link>
    </>
  );
}

const Crumb = () => <HugeiconsIcon icon={ArrowRight01Icon} {...stylex.props(styles.crumb, styles.separator)} />;

function Tabs({ project, environment }: { project: string; environment: string }) {
  return (
    <nav {...stylex.props(styles.tabs)}>
      {sections.map((section) => (
        <Link
          key={section.to}
          to={section.to}
          params={{ project, environment }}
          activeOptions={{ exact: true }}
          {...stylex.props(styles.tab)}
          activeProps={stylex.props(styles.active)}
          inactiveProps={stylex.props(styles.inactive)}
        >
          {section.title}
        </Link>
      ))}
    </nav>
  );
}
