import { Outlet } from "@tanstack/react-router";
import { TopNav } from "@/components/top-nav";
import { useSession } from "@/lib/session";
import { SignIn } from "@/routes/sign-in";

export function Layout() {
    const { token } = useSession();
    if (token === null) return <SignIn />;

    return (
        <div className="flex min-h-svh flex-col">
            <TopNav />
            <Outlet />
        </div>
    );
}
