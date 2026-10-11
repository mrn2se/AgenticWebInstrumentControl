import { defineConfig } from "vitest/config";
import tailwindcss from "@tailwindcss/vite";

// Dev server proxies /ws, /api and /mcp to the awic-server backend (SPECIFICATION.md
// §11). Production is served by the Rust binary out of web/dist, which has no
// proxy — the app talks to whatever origin it was loaded from (see
// src/main.ts's wsUrl()).
export default defineConfig({
  plugins: [tailwindcss()],
  server: {
    port: 5173,
    proxy: {
      "/ws": {
        target: "http://127.0.0.1:8080",
        ws: true,
        changeOrigin: true,
      },
      "/api": {
        target: "http://127.0.0.1:8080",
        changeOrigin: true,
      },
      // So the MCP tab's endpoint URL (this page's origin + /mcp) also works
      // while developing against the dev server.
      "/mcp": {
        target: "http://127.0.0.1:8080",
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: "dist",
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
