import { AUTH_USER_HEADER, type Env } from "./env";
import { verifyBrowserToken } from "./auth";
import { exchangeWithVerifier, revokeSession } from "./workos";
import {
  SESSION_COOKIE,
  DEV_SESSION_COOKIE,
  PREVIEW_SESSION_COOKIE,
  SESSION_HASH_HEADER,
  SESSION_ROOM_HEADER,
  TRANSACTION_COOKIE,
  browserActivity,
  consumeBrowserTransaction,
  createBrowserSession,
  createBrowserTransaction,
  createPreviewSession,
  listBrowserSessions,
  listBrowserDevices,
  revokeAllBrowserSessions,
  revokeBrowserSession,
  token,
  tokenHash,
  validateBrowserSession,
  validatePreviewSession
} from "./browser-sessions";

const PKCE = /^[A-Za-z0-9._~-]{43,128}$/;
const noStore = { "cache-control": "no-store", "content-type": "application/json" };
const json = (value: unknown, status = 200, headers?: HeadersInit) => new Response(JSON.stringify(value), { status, headers: { ...noStore, ...headers } });
const configured = (env: Env): env is Env & { WORKOS_API_KEY: string; WORKOS_BROWSER_ORIGIN: string; WORKOS_BROWSER_OWNER_SUBJECT: string; BROWSER_SESSION_KEY: string; WORKOS_ISSUER: string; WORKOS_JWKS_URL: string } => Boolean(env.WORKOS_API_KEY && env.WORKOS_BROWSER_ORIGIN && env.WORKOS_BROWSER_OWNER_SUBJECT && env.BROWSER_SESSION_KEY && env.WORKOS_ISSUER && env.WORKOS_JWKS_URL);

const loopback = (url: URL): boolean => url.hostname === "127.0.0.1" || url.hostname === "localhost" || url.hostname.endsWith(".localhost") || url.hostname === "[::1]";
const devOrigin = (env: Env): string | undefined => {
  if (!env.BROWSER_DEV_ORIGIN) return undefined;
  try { return loopback(new URL(env.BROWSER_DEV_ORIGIN)) ? env.BROWSER_DEV_ORIGIN : undefined; } catch { return undefined; }
};
const devBrowser = (env: Env, _url: URL): env is Env & { BROWSER_DEV_OWNER_SUBJECT: string; BROWSER_SESSION_KEY: string } =>
  // Wrangler's local proxy presents an internal request URL; the dev worker itself is bound to 127.0.0.1 and every sensitive request still requires the exact configured loopback Origin.
  env.AUTH_MODE === "dev" && Boolean(devOrigin(env) && env.BROWSER_DEV_OWNER_SUBJECT && env.BROWSER_SESSION_KEY);
const sessionCookie = (name: string, value: string, maxAge: number, secure: boolean): string =>
  `${name}=${value};${secure ? " Secure;" : ""} HttpOnly; Path=/; SameSite=Strict; Max-Age=${maxAge}`;
const cookies = (request: Request): Record<string, string> => Object.fromEntries((request.headers.get("cookie") ?? "").split(";").map((item) => { const at = item.indexOf("="); return at < 0 ? ["", ""] : [item.slice(0, at).trim(), item.slice(at + 1).trim()]; }));
const callbackUrl = (origin: string) => `${origin}/api/browser/callback`;
const cookie = (name: string, value: string, sameSite: "Strict" | "Lax", maxAge: number) => `${name}=${value}; Secure; HttpOnly; Path=/; SameSite=${sameSite}; Max-Age=${maxAge}`;
const clearCookie = (name: string) => cookie(name, "", "Strict", 0);
const validOrigin = (request: Request, origin: string) => request.headers.get("origin") === origin;


const trustedDevProxy = (request: Request, env: Env) => Boolean(env.BROWSER_DEV_PROXY_KEY && request.headers.get("x-comet-dev-proxy") === env.BROWSER_DEV_PROXY_KEY);

const devRequest = (request: Request, env: Env, url: URL) => devBrowser(env, url) && (loopback(url) || trustedDevProxy(request, env));
const codeChallenge = async (verifier: string): Promise<string> => { const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier))); let binary = ""; for (const byte of hash) binary += String.fromCharCode(byte); return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, ""); };

const redirectWithCookies = (location: string, values: string[]): Response => {
  const headers = new Headers({ location, "cache-control": "no-store" });
  for (const value of values) headers.append("set-cookie", value);
  return new Response(null, { status: 303, headers });
};

const sessionCookieName = (env: Env, url: URL) => devBrowser(env, url) ? DEV_SESSION_COOKIE : SESSION_COOKIE;
const session = async (request: Request, env: Env, url: URL) => {
  const all = cookies(request);
  const raw = all[sessionCookieName(env, url)] ?? all[SESSION_COOKIE];
  return raw ? validateBrowserSession(env, await tokenHash(raw), false, true) : undefined;
};
const localSession = async (request: Request, env: Env, url: URL) => {
  const all = cookies(request);
  const raw = all[sessionCookieName(env, url)] ?? all[SESSION_COOKIE];
  return raw ? validateBrowserSession(env, await tokenHash(raw), false, false) : undefined;
};
const csrfSession = async (request: Request, env: Env, url: URL) => {
  const localDev = devRequest(request, env, url);
  const expectedOrigin = localDev ? devOrigin(env) : env.WORKOS_BROWSER_ORIGIN;
  if (!expectedOrigin || !(validOrigin(request, expectedOrigin) || (localDev && trustedDevProxy(request, env)))) return undefined;
  const found = await localSession(request, env, url);
  return found && request.headers.get("x-csrf-token") === found.csrfToken ? found : undefined;
};

/** Browser UI API. Provider credentials remain only inside BrowserSessionStore. */
export const handleBrowserRoute = async (request: Request, env: Env, url: URL): Promise<Response | undefined> => {
  if (!url.pathname.startsWith("/api/browser/")) return undefined;
  const localDev = devRequest(request, env, url);
  if (!configured(env) && !localDev) return json({ error: "browser_auth_not_configured" }, 501);
  const path = url.pathname;

  // Deliberately separate from WorkOS: only explicit AUTH_MODE=dev on a
  // loopback origin can mint this test cookie, and its owner must be supplied.
  if (path === "/api/browser/dev-login" && request.method === "POST") {
    const ownerId = env.BROWSER_DEV_OWNER_SUBJECT;
    if (!localDev || !ownerId || !(validOrigin(request, devOrigin(env)!) || trustedDevProxy(request, env))) return json({ error: "forbidden" }, 403);
    const raw = token();
    const csrf = token();
    const created = await createBrowserSession(env, {
      hash: await tokenHash(raw),
      ownerId,
      providerSessionId: `dev-${ownerId}`,
      csrfToken: csrf,
      ...(env.BROWSER_DEV_ORGANIZATION_ID ? { organizationId: env.BROWSER_DEV_ORGANIZATION_ID } : {}),
      accessToken: "development-only",
      refreshToken: "development-only",
      providerExpiresAt: Date.now() + 30 * 24 * 60 * 60 * 1000
    });
    if (!created) return json({ error: "session_unavailable" }, 503);
    return json({ authenticated: true, ownerId, csrfToken: csrf }, 200, {
      "set-cookie": sessionCookie(DEV_SESSION_COOKIE, raw, 30 * 24 * 60 * 60, false)
    });
  }
  if (path === "/api/browser/login" && request.method === "POST") {

    if (!configured(env)) return json({ error: "browser_auth_not_configured" }, 501);
    if (!validOrigin(request, env.WORKOS_BROWSER_ORIGIN)) return json({ error: "forbidden" }, 403);
    const state = token();
    const verifier = token() + token().slice(0, 1); // 43 URL-safe chars
    const nonce = token();
    await createBrowserTransaction(env, { state, verifier, nonceHash: await tokenHash(nonce) });
    const query = new URLSearchParams({ client_id: env.WORKOS_CLIENT_ID, response_type: "code", provider: "authkit", redirect_uri: callbackUrl(env.WORKOS_BROWSER_ORIGIN), state, code_challenge: await codeChallenge(verifier), code_challenge_method: "S256" });
    return json({ authorizationUrl: `https://api.workos.com/user_management/authorize?${query}` }, 200, { "set-cookie": cookie(TRANSACTION_COOKIE, nonce, "Lax", 300) });
  }
  if (path === "/api/browser/callback" && request.method === "GET") {

    if (!configured(env)) return json({ error: "browser_auth_not_configured" }, 501);
    const code = url.searchParams.getAll("code");
    const state = url.searchParams.getAll("state");
    const nonce = cookies(request)[TRANSACTION_COOKIE];
    if (code.length !== 1 || state.length !== 1 || !nonce) return json({ error: "invalid_callback" }, 400);
    const consumed = await consumeBrowserTransaction(env, { state: state[0], nonceHash: await tokenHash(nonce) });
    if (typeof consumed?.verifier !== "string" || !PKCE.test(consumed.verifier)) return json({ error: "invalid_callback" }, 401);
    try {
      const result = await exchangeWithVerifier(env, env.WORKOS_API_KEY, code[0]!, consumed.verifier);
      const verified = await verifyBrowserToken(env, result.accessToken);
      if (!verified?.sessionId || !verified.expiresAt || result.user.id !== verified.userId || verified.userId !== env.WORKOS_BROWSER_OWNER_SUBJECT) return json({ error: "forbidden" }, 403);
      const raw = token();
      const csrf = token();
      const created = await createBrowserSession(env, {
        hash: await tokenHash(raw),
        ownerId: verified.userId,
        providerSessionId: verified.sessionId,
        csrfToken: csrf,
        ...(verified.orgId ? { organizationId: verified.orgId } : {}),
        accessToken: result.accessToken,
        refreshToken: result.refreshToken,
        providerExpiresAt: verified.expiresAt
      });
      if (!created) return json({ error: "session_unavailable" }, 503);
      return redirectWithCookies(`${env.WORKOS_BROWSER_ORIGIN}/`, [
        cookie(SESSION_COOKIE, raw, "Strict", 30 * 24 * 60 * 60),
        clearCookie(TRANSACTION_COOKIE)
      ]);
    } catch {
      return json({ error: "authentication_failed" }, 401);
    }
  }
  if (path === "/api/browser/session" && request.method === "GET") {
    const found = await session(request, env, url);
    return json(found ? { authenticated: true, ownerId: found.ownerId, ...(found.organizationId ? { organizationId: found.organizationId } : {}), expiresAt: found.expiresAt, csrfToken: found.csrfToken } : { authenticated: false });
  }
  if (path === "/api/browser/activity" && request.method === "POST") {
    const found = await csrfSession(request, env, url);
    if (!found) return json({ error: "unauthenticated" }, 401);
    const touched = await browserActivity(env, found.hash, found.csrfToken);
    return json({ authenticated: touched?.authenticated === true });
  }
  if (path === "/api/browser/logout" && request.method === "POST") {
    const found = await csrfSession(request, env, url);
    if (!found) return json({ error: "unauthenticated" }, 401);
    await revokeBrowserSession(env, found.ownerId, found.hash);
    if (env.WORKOS_API_KEY) try { await revokeSession(env.WORKOS_API_KEY, found.providerSessionId); } catch { /* local logout remains durable */ }
    return json({ ok: true }, 200, { "set-cookie": sessionCookie(sessionCookieName(env, url), "", 0, !localDev) });
  }
  if (path === "/api/browser/sessions" && request.method === "GET") {
    const found = await session(request, env, url);
    if (!found) return json({ error: "unauthenticated" }, 401);
    const listed = await listBrowserSessions(env, found.ownerId);
    const sessions = Array.isArray(listed?.sessions) ? listed.sessions.map((entry) => ({ ...(entry as object), current: (entry as { hash?: string }).hash === found.hash })) : [];
    return json({ sessions });
  }
  if (path === "/api/browser/devices" && request.method === "GET") {
    const found = await session(request, env, url);
    if (!found) return json({ error: "unauthenticated" }, 401);
    const listed = await listBrowserDevices(env, found.ownerId);
    const entries = Array.isArray(listed?.devices) ? listed.devices.filter((entry): entry is { id: string; name?: string } =>
      typeof entry === "object" && entry !== null && typeof entry.id === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(entry.id) && (entry.name === undefined || typeof entry.name === "string")
    ) : [];
    const devices = await Promise.all(entries.map(async ({ id, name }) => {
      const stub = env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(`d2/${id}`));
      const status = await stub.fetch(new Request("https://device-room/status", { headers: { [AUTH_USER_HEADER]: found.ownerId } }));
      const body = status.ok ? await status.json() as { hostConnected?: unknown } : undefined;
      return { id, ...(name ? { name } : {}), online: body?.hostConnected === true };
    }));
    return json({ devices });
  }

  if (path === "/api/browser/revoke-all" && request.method === "POST") {
    const found = await csrfSession(request, env, url);
    if (!found) return json({ error: "unauthenticated" }, 401);
    await revokeAllBrowserSessions(env, found.ownerId);
    return json({ ok: true }, 200, { "set-cookie": sessionCookie(sessionCookieName(env, url), "", 0, !localDev) });
  }
  const match = path.match(/^\/api\/browser\/sessions\/([a-f0-9]{64})\/revoke$/);
  if (match && request.method === "POST") {
    const found = await csrfSession(request, env, url);
    if (!found) return json({ error: "unauthenticated" }, 401);
    await revokeBrowserSession(env, found.ownerId, match[1]!);
    return json({ ok: true }, 200, match[1] === found.hash ? { "set-cookie": sessionCookie(sessionCookieName(env, url), "", 0, !localDev) } : undefined);
  }
  return undefined;
};

/** Cookie-only device relay. The caller cannot choose host role, connId, or an
 * internal identity/session header. DeviceRoom still enforces host-claimed owner. */
export const browserDeviceRoute = async (request: Request, env: Env, url: URL): Promise<Response | undefined> => {
  const match = url.pathname.match(/^\/api\/browser\/device\/([A-Za-z0-9_-]{1,128})\/ws$/);
  if (!match) return undefined;

  const localDev = devRequest(request, env, url);
  const expectedOrigin = localDev ? devOrigin(env) : env.WORKOS_BROWSER_ORIGIN;
  if (!expectedOrigin || !(validOrigin(request, expectedOrigin) || (localDev && trustedDevProxy(request, env)))) return json({ error: "forbidden" }, 403);
  if (request.method !== "GET" || request.headers.get("upgrade")?.toLowerCase() !== "websocket") return json({ error: "expected_websocket" }, 426);
  const found = await session(request, env, url);
  if (!found) return json({ error: "unauthenticated" }, 401);
  const headers = new Headers(request.headers);
  headers.delete("authorization");
  headers.delete(AUTH_USER_HEADER);
  headers.delete(SESSION_HASH_HEADER);

  headers.delete(SESSION_ROOM_HEADER);
  headers.delete("x-comet-browser-session");
  headers.set(AUTH_USER_HEADER, found.ownerId);
  headers.set(SESSION_HASH_HEADER, found.hash);

  headers.set(SESSION_ROOM_HEADER, `d2/${match[1]}`);
  const target = new URL(request.url);
  target.pathname = "/ws";
  target.search = `?role=client&connId=${encodeURIComponent(crypto.randomUUID())}`;
  const stub = env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(`d2/${match[1]}`));
  return stub.fetch(new Request(target.toString(), { method: "GET", headers }));
};


/** Each capability gets a separate host below this base origin. This makes the
 * capability cookie host-only, and permits `allow-same-origin` without allowing
 * a preview to read app state or another preview's state. */
export const previewOrigin = (env: Pick<Env, "BROWSER_PREVIEW_ORIGIN">): URL | undefined => {
  if (!env.BROWSER_PREVIEW_ORIGIN) return undefined;
  try {
    const origin = new URL(env.BROWSER_PREVIEW_ORIGIN);
    return origin.protocol === "https:" && origin.pathname === "/" && !origin.search && !origin.hash ? origin : undefined;
  } catch { return undefined; }
};
const previewHost = (base: URL, ticketHash: string): string | undefined => {
  const host = `p-${ticketHash.slice(0, 40)}.${base.hostname}`;
  return host.length <= 253 && /^[a-z0-9.-]+$/.test(host) ? host : undefined;
};
const previewRequestHost = (url: URL, base: URL): string | undefined => {
  const suffix = `.${base.hostname}`;
  return url.protocol === base.protocol && url.port === base.port && url.hostname.endsWith(suffix) && /^p-[a-f0-9]{40}$/.test(url.hostname.slice(0, -suffix.length)) ? url.hostname : undefined;
};

/** Wrangler local tunneling retains the original authority in Host but presents
 * its loopback URL to the Worker. Only the authenticated development proxy may
 * recover that authority; production always uses the request URL. */
const previewRequestHostFromRequest = (request: Request, env: Env, url: URL, base: URL): string | undefined => {
  const direct = previewRequestHost(url, base);
  if (direct || !devRequest(request, env, url)) return direct;
  const authority = request.headers.get("x-comet-dev-preview-host");
  if (!authority) return undefined;
  try { return previewRequestHost(new URL(`${base.protocol}//${authority}`), base); } catch { return undefined; }
};
const previewUrl = (base: URL, host: string): URL => {
  const target = new URL(base);
  target.hostname = host;
  target.pathname = "/";
  target.search = "";
  target.hash = "";
  return target;
};
const previewCookie = (value: string, origin: URL) =>
  `${PREVIEW_SESSION_COOKIE}=${value};${origin.protocol === "https:" ? " Secure; SameSite=None;" : " SameSite=Strict;"} HttpOnly; Path=/; Max-Age=900`;
const previewCsp = "sandbox allow-same-origin allow-scripts allow-forms allow-modals allow-popups allow-downloads; base-uri 'none'";

export const browserPreviewRoute = async (request: Request, env: Env, url: URL): Promise<Response | undefined> => {
  const base = previewOrigin(env);
  if (!base) return undefined;
  const requestHost = previewRequestHostFromRequest(request, env, url, base);
  if (requestHost) return servePreview(request, env, url, requestHost);

  const match = url.pathname.match(/^\/api\/browser\/preview\/([A-Za-z0-9_-]{1,128})\/([A-Za-z0-9_-]{1,128})$/);
  if (!match) return undefined;
  const found = await session(request, env, url);
  if (!found) return json({ error: "unauthenticated" }, 401);
  const raw = token();
  const hash = await tokenHash(raw);
  const host = previewHost(base, hash);
  if (!host) return json({ error: "preview_unavailable" }, 503);
  const created = await createPreviewSession(env, hash, found.hash, found.ownerId, match[1]!, match[2]!, host);
  if (!created) return json({ error: "preview_unavailable" }, 503);
  const target = previewUrl(base, host);
  target.searchParams.set("ticket", raw);
  return new Response(null, { status: 302, headers: { location: target.toString(), "cache-control": "no-store", "referrer-policy": "no-referrer", "cross-origin-embedder-policy": "require-corp", "cross-origin-resource-policy": "cross-origin" } });
};

const servePreview = async (request: Request, env: Env, url: URL, host: string): Promise<Response> => {
  const ticket = url.pathname === "/" ? url.searchParams.get("ticket") : undefined;
  if (ticket) {
    const found = await validatePreviewSession(env, await tokenHash(ticket));
    if (!found || found.host !== host) return json({ error: "unauthenticated" }, 401);
    return new Response(null, { status: 302, headers: { location: new URL("/", url).toString(), "cache-control": "no-store", "referrer-policy": "no-referrer", "set-cookie": previewCookie(ticket, url), "cross-origin-embedder-policy": "require-corp", "cross-origin-resource-policy": "cross-origin" } });
  }
  const raw = cookies(request)[PREVIEW_SESSION_COOKIE];
  const found = raw ? await validatePreviewSession(env, await tokenHash(raw)) : undefined;
  if (!found || found.host !== host) return json({ error: "unauthenticated" }, 401);
  const body = await readPreviewBody(request);
  if (!body) return json({ error: "too_large" }, 413);
  const payload = previewFrame({ service: found.serviceId, method: request.method, path: `${url.pathname}${url.search}`, headers: [...request.headers].filter(([name]) => !["cookie", "authorization", "proxy-authorization"].includes(name.toLowerCase())) }, body);
  const headers = new Headers({ [AUTH_USER_HEADER]: found.ownerId, [SESSION_HASH_HEADER]: found.parentHash, [SESSION_ROOM_HEADER]: `d2/${found.deviceId}`, upgrade: "websocket" });
  const stub = env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(`d2/${found.deviceId}`));
  const connected = await stub.fetch(new Request("https://device-room/ws?role=client&connId=" + crypto.randomUUID(), { headers }));
  if (connected.status !== 101 || !connected.webSocket) return json({ error: "preview_unavailable" }, 502);
  const socket = connected.webSocket;
  socket.accept();
  try {
    socket.send(deviceFrame({ s: crypto.randomUUID(), k: "preview" }, payload));
    const response = await waitPreviewResponse(socket);
    const frame = decodeDeviceFrame(response);
    if (frame.header.k !== "preview") throw new Error("invalid preview response");
    const result = decodePreview(frame.payload);
    const output = new Headers({ "cache-control": "no-store", "x-content-type-options": "nosniff", "content-security-policy": previewCsp, "referrer-policy": "no-referrer", "cross-origin-embedder-policy": "require-corp", "cross-origin-resource-policy": "cross-origin" });
    for (const [name, value] of result.headers) if (!["set-cookie", "content-length", "connection", "transfer-encoding", "content-security-policy", "location"].includes(name.toLowerCase())) output.append(name, value);
    const location = result.headers.find(([name]) => name.toLowerCase() === "location")?.[1];
    if (location) output.set("location", rewritePreviewLocation(location, url));
    return new Response(request.method === "HEAD" || [204, 205, 304].includes(result.status) ? null : result.body, { status: result.status, headers: output });
  } catch {
    return json({ error: "preview_unavailable" }, 502);
  } finally { socket.close(); }
}

const readPreviewBody = async (request: Request): Promise<Uint8Array | undefined> => {
  if (!request.body) return new Uint8Array();
  const reader = request.body.getReader(); const chunks: Uint8Array[] = []; let size = 0;
  try { for (;;) { const next = await reader.read(); if (next.done) break; size += next.value.byteLength; if (size > 4 * 1024 * 1024) return undefined; chunks.push(next.value); } }
  finally { reader.releaseLock(); }
  const body = new Uint8Array(size); let offset = 0; for (const chunk of chunks) { body.set(chunk, offset); offset += chunk.byteLength; } return body;
};
const waitPreviewResponse = (socket: WebSocket) => new Promise<Uint8Array>((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("preview timed out")), 15_000);
  socket.addEventListener("message", async event => { clearTimeout(timer); resolve(event.data instanceof ArrayBuffer ? new Uint8Array(event.data) : new Uint8Array(await (event.data as Blob).arrayBuffer())); }, { once: true });
  socket.addEventListener("error", () => { clearTimeout(timer); reject(new Error("preview relay failed")); }, { once: true });
});
export const rewritePreviewLocation = (value: string, current: URL): string => {
  try {
    const location = new URL(value, current);
    return (location.hostname === "localhost" || location.hostname === "127.0.0.1" || location.hostname === "[::1]" || location.hostname.endsWith(".localhost")) ? new URL(`${location.pathname}${location.search}${location.hash}`, current).toString() : location.toString();
  } catch { return "/"; }
};

type PreviewHead = { service: string; method: string; path: string; headers: [string, string][] };
const previewFrame = (head: PreviewHead, body: Uint8Array): Uint8Array => {
  const json = new TextEncoder().encode(JSON.stringify(head)); const result = new Uint8Array(4 + json.length + body.length);
  new DataView(result.buffer).setUint32(0, json.length); result.set(json, 4); result.set(body, 4 + json.length); return result;
};
const decodePreview = (bytes: Uint8Array): { status: number; headers: [string, string][]; body: Uint8Array } => {
  if (bytes.length < 4) throw new Error("truncated preview"); const size = new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0);
  if (size > 64 * 1024 || 4 + size > bytes.length) throw new Error("invalid preview");
  const head = JSON.parse(new TextDecoder().decode(bytes.subarray(4, 4 + size))) as { status: unknown; headers: unknown };
  if (!Number.isInteger(head.status) || (head.status as number) < 100 || (head.status as number) > 599 || !Array.isArray(head.headers)) throw new Error("invalid preview");
  const headers = head.headers.filter((value): value is [string, string] => Array.isArray(value) && typeof value[0] === "string" && typeof value[1] === "string");
  return { status: head.status as number, headers, body: bytes.subarray(4 + size) };
};
const deviceFrame = (header: { s: string; k: string }, payload: Uint8Array): Uint8Array => {
  const json = new TextEncoder().encode(JSON.stringify(header)); const prefix: number[] = []; for (let value = json.length; ; value >>>= 7) { prefix.push((value & 127) | (value > 127 ? 128 : 0)); if (value <= 127) break; }
  const result = new Uint8Array(prefix.length + json.length + payload.length); result.set(prefix); result.set(json, prefix.length); result.set(payload, prefix.length + json.length); return result;
};
const decodeDeviceFrame = (bytes: Uint8Array): { header: { k?: unknown }; payload: Uint8Array } => {
  let offset = 0, length = 0, shift = 0; while (true) { const byte = bytes[offset++]; if (byte === undefined || shift > 28) throw new Error("invalid frame"); length |= (byte & 127) << shift; if (!(byte & 128)) break; shift += 7; }
  if (length > 64 * 1024 || offset + length > bytes.length) throw new Error("invalid frame"); return { header: JSON.parse(new TextDecoder().decode(bytes.subarray(offset, offset + length))) as { k?: unknown }, payload: bytes.subarray(offset + length) };
};