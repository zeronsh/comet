import type { Verified } from "./auth";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER, type Env } from "./env";
/** Route identity comes exclusively from verified JWT claims. */
export function previewRoute(request: Request, env: Pick<Env, "PREVIEW_ROOMS">, auth: Verified): Promise<Response> | Response | undefined {
  const url = new URL(request.url);
  const parts = url.pathname.split("/").filter(Boolean);
  const id = /^[A-Za-z0-9_-]{1,128}$/;
  if (parts[0] !== "preview" || parts.length !== 3 || !id.test(parts[1]) || parts[2] !== "ws") return;
  if (auth.orgId !== parts[1]) return new Response("Forbidden", { status: 403 });
  const device = url.searchParams.get("device") ?? "";
  if (!id.test(device)) return new Response("Invalid device", { status: 400 });
  const room = env.PREVIEW_ROOMS.get(env.PREVIEW_ROOMS.idFromName(`preview1/${parts[1]}/${auth.userId}`));
  url.pathname = "/ws"; url.search = `?device=${device}`;
  const headers = new Headers(request.headers);
  headers.set(AUTH_USER_HEADER, auth.userId); headers.delete(ROOM_KIND_HEADER);
  return room.fetch(new Request(url, { method: request.method, headers }));
}
