import { previewRoute } from "../../src/preview-route";
export { PreviewRoom } from "../../src/preview-room";
import { DurableObject } from "cloudflare:workers";

/** Bare SQLite-backed DO; tests reach its real `ctx.storage.sql` via
 * `runInDurableObject` (the cloudflare-os TEST_OVERSEER pattern). */
export class TestLogRoom extends DurableObject {}

export default {
  fetch(request: Request, env: { PREVIEW_ROOMS: DurableObjectNamespace }): Response | Promise<Response> {
    // Test credentials exercise the production routing seam without loading
    // the unrelated session-room WASM inside the Workers test runner.
    const bearer = request.headers.get("authorization");
    if (!bearer?.startsWith("Bearer ")) return new Response("Unauthorized", { status: 401 });
    const [userId, orgId] = bearer.slice(7).split("@");
    return previewRoute(request, env, { userId, orgId }) ?? new Response("test fixture", { status: 404 });
  }
};
