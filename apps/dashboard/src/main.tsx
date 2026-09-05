import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { MotionConfig } from "motion/react";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { SessionProvider } from "@/lib/session";
import { router } from "./router";
import "./styles.css";

const queryClient = new QueryClient();
const root = document.getElementById("root");
if (!root) throw new Error("Dashboard root is missing");

createRoot(root).render(
    <StrictMode>
        <QueryClientProvider client={queryClient}>
            <MotionConfig reducedMotion="user">
                <SessionProvider>
                    <RouterProvider router={router} />
                </SessionProvider>
            </MotionConfig>
        </QueryClientProvider>
    </StrictMode>,
);
