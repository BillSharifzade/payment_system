import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Served by Caddy at /admin in production; the dev server proxies API calls to
// a locally running payment-server.
export default defineConfig({
  base: "/admin/",
  plugins: [react()],
  server: {
    proxy: {
      "/v1": "http://localhost:8099",
      "/health": "http://localhost:8099",
    },
  },
  test: {
    // Pure helpers run in plain node; DOM tests opt in per file with
    // `// @vitest-environment happy-dom`.
    environment: "node",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
