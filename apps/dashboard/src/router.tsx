import { createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { Layout } from "@/components/layout";
import { sections } from "@/lib/format";
import { Deployment, Health, Section } from "@/routes/deployment";
import { Missing, Project } from "@/routes/project";
import { Projects } from "@/routes/projects";

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

const sectionRoutes = [
    section("/", Health),
    ...sections
        .filter((entry) => entry.slug !== "")
        .map((entry) => section(entry.slug, () => <Section title={entry.title} />)),
];

export const router = createRouter({
    routeTree: rootRoute.addChildren([
        projectsRoute,
        projectRoute,
        deploymentRoute.addChildren(sectionRoutes),
    ]),
});

declare module "@tanstack/react-router" {
    interface Register {
        router: typeof router;
    }
}
