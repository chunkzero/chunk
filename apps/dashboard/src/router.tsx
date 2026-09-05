import { createRootRoute, createRoute, createRouter, Link, Outlet } from "@tanstack/react-router";
import { Overview } from "./routes/overview";

const rootRoute = createRootRoute({
    component: () => (
        <div className="shell">
            <header>
                <Link to="/" className="brand">
                    chunk<span> / dashboard</span>
                </Link>
                <span className="badge">Self-hosted</span>
            </header>
            <main>
                <Outlet />
            </main>
            <footer>Your application. Your infrastructure.</footer>
        </div>
    ),
    notFoundComponent: () => (
        <section>
            <h1>Page not found</h1>
            <Link to="/">Back to overview</Link>
        </section>
    ),
});

const overviewRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/",
    component: Overview,
});
export const router = createRouter({ routeTree: rootRoute.addChildren([overviewRoute]) });

declare module "@tanstack/react-router" {
    interface Register {
        router: typeof router;
    }
}
