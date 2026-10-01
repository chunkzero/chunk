import * as stylex from "@stylexjs/stylex";
import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { queryClient } from "./lib/client.ts";
import { router } from "./router.tsx";
import { colors, fonts } from "./tokens.stylex.ts";

import "./styles.css";

const styles = stylex.create({
  body: { backgroundColor: colors.background, color: colors.foreground, fontFamily: fonts.sans },
});

const root = document.getElementById("root");
if (!root) throw new Error("the dashboard root is missing");

document.body.className = stylex.props(styles.body).className ?? "";

createRoot(root).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
);
