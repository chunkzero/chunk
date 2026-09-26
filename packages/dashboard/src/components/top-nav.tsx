import { ArrowRight01Icon, Logout01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Link, useParams } from "@tanstack/react-router";

import { signOut } from "../lib/client.ts";
import { useEnvironment, usePrincipal, useProject } from "../lib/queries.ts";
import { Logo } from "./logo.tsx";
import { Button } from "./ui/button.tsx";

const sections = [
  { to: "/p/$project/$environment", title: "Deployments" },
  { to: "/p/$project/$environment/logs", title: "Logs" },
  { to: "/p/$project/$environment/secrets", title: "Secrets" },
  { to: "/p/$project/$environment/domains", title: "Domains" },
] as const;

export function TopNav() {
  const params = useParams({ strict: false });
  const principal = usePrincipal();

  return (
    <header className="border-b bg-card">
      <div className="mx-auto flex h-13 max-w-6xl items-center justify-between gap-4 px-6">
        <nav
          aria-label="Breadcrumb"
          className="flex min-w-0 items-center gap-2 overflow-x-auto text-sm whitespace-nowrap [&>*]:shrink-0"
        >
          <Link to="/">
            <Logo />
          </Link>
          <span className="hidden text-muted-foreground sm:inline">{window.location.hostname}</span>
          {params.project && <ProjectCrumb project={params.project} />}
          {params.project && params.environment && (
            <EnvironmentCrumb project={params.project} environment={params.environment} />
          )}
        </nav>
        <div className="flex shrink-0 items-center gap-2">
          <span className="hidden text-sm text-muted-foreground sm:inline">
            {principal.data?.principal?.displayName}
          </span>
          <Button variant="ghost" size="sm" className="text-muted-foreground" onClick={signOut}>
            <HugeiconsIcon icon={Logout01Icon} />
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
      <Link to="/p/$project" params={{ project }} className="font-medium">
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
      <Link to="/p/$project/$environment" params={{ project, environment }} className="font-medium">
        {data?.name ?? "…"}
      </Link>
    </>
  );
}

const Crumb = () => <HugeiconsIcon icon={ArrowRight01Icon} className="size-4 text-muted-foreground/60" />;

function Tabs({ project, environment }: { project: string; environment: string }) {
  return (
    <nav className="mx-auto flex max-w-6xl gap-1 overflow-x-auto px-4 text-sm">
      {sections.map((section) => (
        <Link
          key={section.to}
          to={section.to}
          params={{ project, environment }}
          activeOptions={{ exact: true }}
          className="relative px-2 py-2.5"
          activeProps={{ className: "after:absolute after:inset-x-0 after:-bottom-px after:h-0.5 after:bg-foreground" }}
          inactiveProps={{ className: "text-muted-foreground hover:text-foreground" }}
        >
          {section.title}
        </Link>
      ))}
    </nav>
  );
}
