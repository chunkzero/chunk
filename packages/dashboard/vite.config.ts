import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Matches what the management service sends with the built dashboard.
const headers = { "Content-Security-Policy": "frame-ancestors 'none'", "X-Content-Type-Options": "nosniff" };
const management = process.env.CHUNK_MANAGEMENT_URL ?? "http://127.0.0.1:8080";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: { strictPort: true, headers, proxy: { "^/chunk\\.management\\.v1\\.": management } },
  preview: { headers },
});
