import { createRootRoute, createRoute, createRouter } from "@tanstack/react-router";

import { Layout } from "./components/layout.tsx";
import { Environment } from "./routes/environment.tsx";
import { Deployments } from "./routes/environment/deployments.tsx";
import { Domains } from "./routes/environment/domains.tsx";
import { Logs } from "./routes/environment/logs.tsx";
import { Secrets } from "./routes/environment/secrets.tsx";
import { Login } from "./routes/login.tsx";
import { Missing } from "./routes/missing.tsx";
import { Project } from "./routes/project.tsx";
import { Projects } from "./routes/projects.tsx";
import { SignedIn, takeHandoff } from "./routes/signed-in.tsx";

const rootRoute = createRootRoute({ component: Layout, notFoundComponent: Missing });

const environmentRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/p/$project/$environment",
  component: Environment,
});

function section(path: string, component: () => React.ReactNode) {
  return createRoute({ getParentRoute: () => environmentRoute, path, component });
}

export const routeTree = rootRoute.addChildren([
  createRoute({ getParentRoute: () => rootRoute, path: "/", component: Projects }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/login",
    validateSearch: (search: Record<string, unknown>): { code?: string } =>
      typeof search.code === "string" ? { code: search.code } : {},
    component: Login,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/signed-in",
    validateSearch: (search: Record<string, unknown>): { return?: string } =>
      typeof search.return === "string" ? { return: search.return } : {},
    beforeLoad: ({ location, search }) => takeHandoff(location.hash, search.return),
    component: SignedIn,
  }),
  createRoute({ getParentRoute: () => rootRoute, path: "/p/$project", component: Project }),
  environmentRoute.addChildren([
    section("/", Deployments),
    section("logs", Logs),
    section("secrets", Secrets),
    section("domains", Domains),
  ]),
]);

export const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
