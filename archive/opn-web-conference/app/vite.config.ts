import { fileURLToPath } from "node:url";
import { defineConfig, loadEnv } from "vite";
import tailwindcss from "@tailwindcss/vite";

// Dev topology: this server serves the app and proxies four paths so the browser
// talks same-origin — no CORS, no API key, no core URL in the browser.
//   /join  (HTTP)      → the dev-auth sidecar (holds the tenant key)
//   /rooms (HTTP)      → the dev-auth sidecar (lobby roster; W1)
//   /v1    (HTTP)      → Core's REST (channel history; JWT-authed by the browser)
//   /ws    (WebSocket) → Core's gateway
//
// Config values come from the monorepo-root `.env` (loaded below), falling back
// to the shell environment, then a local-dev default. Only OPN_CORE_URL and
// DEV_AUTH_PORT are read here; the tenant key is never touched by the app.
const repoRoot = fileURLToPath(new URL("..", import.meta.url));

export default defineConfig(({ mode }) => {
  const env = { ...loadEnv(mode, repoRoot, ""), ...process.env };
  const coreUrl = env.OPN_CORE_URL ?? "http://localhost:8080";
  const devAuthPort = env.DEV_AUTH_PORT ?? "8787";
  // http://→ws://, https://→wss:// (https starts with http, so this covers both).
  const wsTarget = coreUrl.replace(/^http/, "ws");

  return {
    plugins: [tailwindcss()],
    server: {
      proxy: {
        "/join": { target: "http://localhost:" + devAuthPort, changeOrigin: true },
        "/rooms": { target: "http://localhost:" + devAuthPort, changeOrigin: true },
        "/v1": { target: coreUrl, changeOrigin: true },
        "/ws": { target: wsTarget, ws: true, changeOrigin: true },
      },
    },
  };
});
