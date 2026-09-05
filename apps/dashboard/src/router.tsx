import { createRootRoute, createRoute, createRouter, redirect } from "@tanstack/react-router";
import { Layout } from "@/components/layout";
import { Deployment } from "@/routes/deployment";
import { Missing, Project } from "@/routes/project";
import { Projects } from "@/routes/projects";
import { Server } from "@/routes/sections/health";
import { Application, Assets, Players } from "@/routes/sections/overviews";
import { Settings } from "@/routes/sections/settings";

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
        createRoute({ getParentRoute: () => rootRoute, path: "/server", component: Server }),
        projectRoute,
        deploymentRoute.addChildren([
            section("/", Application),
            section("players", Players),
            section("assets", Assets),
            section("settings", Settings),
            ...(["sessions", "functions"] as const).map((path) =>
                createRoute({
                    getParentRoute: () => deploymentRoute,
                    path,
                    beforeLoad: ({ params }) => {
                        throw redirect({ to: "/p/$project/$deployment", params });
                    },
                }),
            ),
            createRoute({
                getParentRoute: () => deploymentRoute,
                path: "logs",
                beforeLoad: () => {
                    throw redirect({ to: "/server" });
                },
            }),
        ]),
    ]),
});

declare module "@tanstack/react-router" {
    interface Register {
        router: typeof router;
    }
}
