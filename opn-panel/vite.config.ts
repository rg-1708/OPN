import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Dev topology (opn-panel-roadmap.md §Architecture): Vite serves the SPA and
// proxies `/admin` to Core's private admin bind so the browser is same-origin —
// no CORS, no admin secrets in the browser. In prod the built `dist/` is served
// by the admin bind itself (ADMIN_PANEL_DIR), same origin either way.
export default defineConfig(({ mode }) => {
  const env = { ...loadEnv(mode, process.cwd(), ""), ...process.env };
  const adminBind = env.ADMIN_BIND_URL ?? "http://127.0.0.1:9091";
  return {
    plugins: [react(), tailwindcss()],
    server: {
      proxy: {
        "/admin": { target: adminBind, changeOrigin: true },
      },
    },
  };
});
