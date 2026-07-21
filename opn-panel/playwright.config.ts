import { defineConfig, devices } from "@playwright/test";

// Smoke against a running dev stack (Core admin bind up + `npm run dev`).
// PANEL_URL points at the Vite dev server; ADMIN_PASSWORD is the operator
// password whose argon2 hash is in Core's ADMIN_PASSWORD_HASH.
export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  reporter: "list",
  use: {
    baseURL: process.env.PANEL_URL ?? "http://localhost:5173",
    trace: "on-first-retry",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
