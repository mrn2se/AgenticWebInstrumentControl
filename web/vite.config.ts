import { defineConfig } from "vitest/config";
import tailwindcss from "@tailwindcss/vite";

// Dev server proxies /ws, /api and /mcp to the awic-server backend (SPECIFICATION.md
// §11). Production is served by the Rust binary out of web/dist, which has no
// proxy — the app talks to whatever origin it was loaded from
// (src/backend/backend.ts's selfBackend()).
//
// `--mode pages` is the same app for GitHub Pages: relative asset paths for a
// project page's subpath, and no server to talk to, so it opens with the
// welcome dialog and the simulated bench (src/main.ts).
export default defineConfig(({ mode }) => ({
  base: mode === "pages" ? "./" : "/",
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
    outDir: mode === "pages" ? "dist-pages" : "dist",
    // Only the public site has operator details; a user's own server has none to show.
    ...(mode === "pages" ? { rollupOptions: { input: ["index.html", "legal.html"] } } : {}),
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
}));
