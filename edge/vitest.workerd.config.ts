import { defineConfig } from "vitest/config";
import { cloudflareTest } from "@cloudflare/vitest-pool-workers";

// Runtime-real test tier: runs inside actual workerd via
// @cloudflare/vitest-pool-workers, against a real SQLite-backed Durable
// Object, so platform limits like the ~2MB SQLITE_TOOBIG row cap (the
// 2026-08-05 whale sync freeze) are the runtime's own, not FakeSql constants.
// `npm run test:workerd`.
//
// loro-wasm does NOT work in this tier: the pool's test runner evaluates
// modules where wasm codegen is disallowed, while a real worker compiles the
// base64-inlined module at startup (deployed edge and `wrangler dev` are
// fine). loro-on-workerd coverage lives in the wrangler-dev scripts
// (scripts/whale-check.mjs, scripts/fold-check.mjs).
export default defineConfig({
  plugins: [
    cloudflareTest({
      main: "./test/workerd/fixture.ts",
      miniflare: {
        compatibilityDate: "2026-07-01",
        durableObjects: {
          TEST_LOG: { className: "TestLogRoom", useSQLite: true },
          PREVIEW_ROOMS: { className: "PreviewRoom", useSQLite: true },
          BROWSER_SESSIONS: { className: "BrowserSessionStore", useSQLite: true }
,
          DEVICE_ROOMS: { className: "DeviceRoom", useSQLite: true }
        },
        bindings: {
          AUTH_MODE: "dev",
          WORKOS_CLIENT_ID: "client_test",
          WORKOS_API_KEY: "test-only",
          WORKOS_ISSUER: "https://issuer.test",
          WORKOS_JWKS_URL: "https://issuer.test/jwks",
          WORKOS_BROWSER_OWNER_SUBJECT: "browser-e2e-owner",
          BROWSER_DEV_OWNER_SUBJECT: "browser-e2e-owner",

          BROWSER_DEV_ORGANIZATION_ID: "dev-org",

          BROWSER_DEV_ORIGIN: "http://localhost",
          BROWSER_SESSION_KEY: "workerd-test-session-key",
          WORKOS_BROWSER_ORIGIN: "https://test",
          BROWSER_PREVIEW_ORIGIN: "https://preview.test"
        }
      }
    })
  ],
  resolve: {
    // Mirror wrangler.jsonc: workerd cannot fetch loro's WASM by URL; the
    // base64 entry inlines it.
    alias: { "loro-crdt": "loro-crdt/base64" }
  },
  test: {
    include: ["test/workerd/**/*.test.ts"]
  }
});
