import { previewRoute } from "../../src/preview-route";
export { PreviewRoom } from "../../src/preview-room";

export { BrowserSessionStore } from "../../src/browser-sessions";

import { browserDeviceRoute, browserPreviewRoute, handleBrowserRoute } from "../../src/browser-routes";
import type { Env } from "../../src/env";

export { DeviceRoom } from "../../src/device-room";
import { DurableObject } from "cloudflare:workers";

/** Bare SQLite-backed DO; tests reach its real `ctx.storage.sql` via
 * `runInDurableObject` (the cloudflare-os TEST_OVERSEER pattern). */
export class TestLogRoom extends DurableObject {}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const browser = await browserDeviceRoute(request, env, new URL(request.url));
    if (browser) return browser;
    const preview = await browserPreviewRoute(request, env, new URL(request.url));
    if (preview) return preview;

    const browserApi = await handleBrowserRoute(request, env, new URL(request.url));
    if (browserApi) return browserApi;
    // Test credentials exercise the production routing seam without loading
    // the unrelated session-room WASM inside the Workers test runner.
    const bearer = request.headers.get("authorization");
    if (!bearer?.startsWith("Bearer ")) return new Response("Unauthorized", { status: 401 });
    const [userId, orgId] = bearer.slice(7).split("@");
    return previewRoute(request, env, { userId, orgId }) ?? new Response("test fixture", { status: 404 });
  }
};
