import { createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { Layout } from "@/components/layout";
import { Deployment } from "@/routes/deployment";
import { Missing, Project } from "@/routes/project";
import { Projects } from "@/routes/projects";
import { Health } from "@/routes/sections/health";
import { Logs } from "@/routes/sections/logs";
import { Assets, Functions, Players, Sessions, Settings } from "@/routes/sections/overviews";

const rootRoute = createRootRoute({ component: Layout, notFoundComponent: Missing });

const projectsRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/",
    component: Projects,
});
const projectRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/p/$project",
    component: Project,
});
const deploymentRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/p/$project/$deployment",
    component: Deployment,
});

function section<const Path extends string>(path: Path, component: () => React.ReactNode) {
    return createRoute({ getParentRoute: () => deploymentRoute, path, component });
}

export const router = createRouter({
    routeTree: rootRoute.addChildren([
        projectsRoute,
        projectRoute,
        deploymentRoute.addChildren([
            section("/", Health),
            section("functions", Functions),
            section("sessions", Sessions),
            section("players", Players),
            section("assets", Assets),
            section("logs", Logs),
            section("settings", Settings),
        ]),
    ]),
});

declare module "@tanstack/react-router" {
    interface Register {
        router: typeof router;
    }
}
