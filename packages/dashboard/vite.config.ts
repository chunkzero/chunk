import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    strictPort: true,
    proxy: { "^/chunk\\.management\\.v1\\.": process.env.CHUNK_MANAGEMENT_URL ?? "http://127.0.0.1:8080" },
  },
});
