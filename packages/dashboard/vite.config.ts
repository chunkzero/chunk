import stylex from "@stylexjs/unplugin";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Matches what the management service sends with the built dashboard.
const headers = { "Content-Security-Policy": "frame-ancestors 'none'", "X-Content-Type-Options": "nosniff" };
const management = process.env.CHUNK_MANAGEMENT_URL ?? "http://127.0.0.1:8080";

// Tests only need StyleX compiled away; the Vite adapter's dev-server polling would keep Vitest from exiting.
const styleX = process.env.VITEST ? stylex.rollup() : stylex.vite({ lightningcssOptions: { minify: true } });

export default defineConfig({
  plugins: [styleX, react()],
  // Every supported browser has modulepreload, and the polyfill would ship es-module-shims code in the bundle.
  build: { modulePreload: { polyfill: false } },
  server: { strictPort: true, headers, proxy: { "^/chunk\\.management\\.v1\\.": management } },
  preview: { headers },
});
