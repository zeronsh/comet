import { verifyBrowserToken } from "./auth";
import type { Env } from "./env";
import { WorkOsAuthFailed, refresh } from "./workos";

export const SESSION_COOKIE = "__Host-comet_session";

/** Non-Secure cookie used only by explicit loopback AUTH_MODE=dev. */
export const DEV_SESSION_COOKIE = "comet_dev_session";
export const TRANSACTION_COOKIE = "__Host-comet_auth_txn";
export const SESSION_STORE_NAME = "browser-sessions-v1";
export const SESSION_HEADER = "x-comet-browser-session";
export const SESSION_HASH_HEADER = "x-comet-browser-session-hash";
export const SESSION_ROOM_HEADER = "x-comet-browser-room";
export const SESSION_STORE_HEADER = "x-comet-browser-session-store";
export const PREVIEW_SESSION_COOKIE = "__Host-comet_preview";

const DAY = 24 * 60 * 60 * 1000;
const MAX_AGE = 30 * DAY;
const IDLE_AGE = 7 * DAY;
const REFRESH_EARLY = 60_000;

const NO_RETRY = 8_640_000_000_000_000;
const text = new TextEncoder();
const decoder = new TextDecoder();

type Json = Record<string, unknown>;
export interface BrowserSession {
  readonly hash: string;
  readonly ownerId: string;
  readonly providerSessionId: string;
  readonly csrfToken: string;
  readonly expiresAt: number;
  readonly generation: number;
  readonly organizationId?: string;
}


export interface PreviewSession {
  readonly hash: string;
  readonly ownerId: string;
  readonly deviceId: string;
  readonly serviceId: string;
  /** Exact isolated hostname assigned to this capability. */
  readonly host: string;

  readonly expiresAt: number;
  /** Internal-only parent session hash used when opening the DeviceRoom. */
  readonly parentHash: string;
}

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json", "cache-control": "no-store" } });
const record = (value: unknown): value is Json => typeof value === "object" && value !== null && !Array.isArray(value);
const string = (value: unknown, max = 4096): value is string => typeof value === "string" && value.length > 0 && value.length <= max;

export const token = (): string => {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
};

export const tokenHash = async (value: string): Promise<string> => {
  const bytes = new Uint8Array(await crypto.subtle.digest("SHA-256", text.encode(value)));
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
};

const body = async (request: Request): Promise<Json | undefined> => {
  const length = Number(request.headers.get("content-length") ?? "0");
  if (!Number.isSafeInteger(length) || length > 16 * 1024) return undefined;
  try {
    const value: unknown = await request.json();
    return record(value) ? value : undefined;
  } catch {
    return undefined;
  }
};

const seal = async (keyMaterial: string, value: string): Promise<string> => {
  const keyBytes = await crypto.subtle.digest("SHA-256", text.encode(keyMaterial));
  const key = await crypto.subtle.importKey("raw", keyBytes, "AES-GCM", false, ["encrypt"]);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const cipher = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, text.encode(value)));
  const bytes = new Uint8Array(iv.length + cipher.length);
  bytes.set(iv);
  bytes.set(cipher, iv.length);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
};
const open = async (keyMaterial: string, value: string): Promise<string> => {
  const bytes = Uint8Array.from(atob(value), (char) => char.charCodeAt(0));
  const keyBytes = await crypto.subtle.digest("SHA-256", text.encode(keyMaterial));
  const key = await crypto.subtle.importKey("raw", keyBytes, "AES-GCM", false, ["decrypt"]);
  return decoder.decode(await crypto.subtle.decrypt({ name: "AES-GCM", iv: bytes.slice(0, 12) }, key, bytes.slice(12)));
};

export class BrowserSessionStore implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;

  private readonly refreshing = new Map<string, Promise<SessionRow | undefined>>();
  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS browser_sessions (hash TEXT PRIMARY KEY, owner TEXT NOT NULL, sid TEXT NOT NULL, csrf TEXT NOT NULL, organization_id TEXT, access_secret TEXT NOT NULL, refresh_secret TEXT NOT NULL, provider_expiry INTEGER NOT NULL, created INTEGER NOT NULL, last_active INTEGER NOT NULL, absolute_expiry INTEGER NOT NULL, generation INTEGER NOT NULL DEFAULT 1, revoked INTEGER NOT NULL DEFAULT 0)");
    try { ctx.storage.sql.exec("ALTER TABLE browser_sessions ADD COLUMN organization_id TEXT"); } catch { /* existing store */ }
    ctx.storage.sql.exec("CREATE INDEX IF NOT EXISTS browser_sessions_owner ON browser_sessions(owner, revoked)");
    ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS browser_transactions (state TEXT PRIMARY KEY, nonce_hash TEXT NOT NULL, verifier TEXT NOT NULL, expires INTEGER NOT NULL, consumed INTEGER NOT NULL DEFAULT 0)");

    ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS browser_bindings (hash TEXT NOT NULL, room TEXT NOT NULL, conn_id TEXT NOT NULL, PRIMARY KEY(hash, room, conn_id))");

    try { ctx.storage.sql.exec("ALTER TABLE browser_bindings ADD COLUMN next_retry INTEGER NOT NULL DEFAULT 0"); } catch { /* existing store */ }

    ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS preview_sessions (hash TEXT PRIMARY KEY, parent_hash TEXT NOT NULL, owner TEXT NOT NULL, device_id TEXT NOT NULL, service_id TEXT NOT NULL, host TEXT NOT NULL, expires INTEGER NOT NULL)");
    try { ctx.storage.sql.exec("ALTER TABLE preview_sessions ADD COLUMN parent_hash TEXT"); } catch { /* current schema */ }
    try { ctx.storage.sql.exec("ALTER TABLE preview_sessions ADD COLUMN host TEXT"); } catch { /* current schema */ }
    ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS browser_devices (owner TEXT NOT NULL, device_id TEXT NOT NULL, name TEXT, seen_at INTEGER NOT NULL, PRIMARY KEY(owner, device_id))");
    try { ctx.storage.sql.exec("ALTER TABLE browser_devices ADD COLUMN name TEXT"); } catch { /* existing store */ }
  }

  async fetch(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    const input = await body(request);
    if (!input) return json({ error: "invalid_request" }, 400);
    if (path === "/transaction") return this.transaction(input);
    if (path === "/consume-transaction") return this.consumeTransaction(input);
    if (path === "/create") return this.create(input);
    if (path === "/validate") return this.validate(input);
    if (path === "/activity") return this.activity(input);
    if (path === "/list") return this.list(input);
    if (path === "/revoke") return this.revoke(input);
    if (path === "/revoke-all") return this.revokeAll(input);

    if (path === "/bind") return this.bind(input);
    if (path === "/unbind") return this.unbind(input);
    if (path === "/preview-create") return this.previewCreate(input);
    if (path === "/preview-validate") return this.previewValidate(input);

    if (path === "/register-device") return this.registerDevice(input);
    if (path === "/devices") return this.devices(input);
    return json({ error: "not_found" }, 404);
  }

  private async transaction(input: Json): Promise<Response> {
    if (!string(input.state, 256) || !string(input.nonceHash, 128) || !string(input.verifier, 128)) return json({ error: "invalid_request" }, 400);
    const expires = Date.now() + 5 * 60_000;
    this.ctx.storage.sql.exec("INSERT INTO browser_transactions(state, nonce_hash, verifier, expires) VALUES (?, ?, ?, ?)", input.state, input.nonceHash, input.verifier, expires);
    await this.schedule();
    return json({ ok: true });
  }
  private async consumeTransaction(input: Json): Promise<Response> {
    if (!string(input.state, 256) || !string(input.nonceHash, 128)) return json({ error: "invalid_request" }, 400);
    const row = [...this.ctx.storage.sql.exec("SELECT verifier, nonce_hash, expires, consumed FROM browser_transactions WHERE state = ?", input.state)][0] as { verifier: string; nonce_hash: string; expires: number; consumed: number } | undefined;
    this.ctx.storage.sql.exec("DELETE FROM browser_transactions WHERE state = ?", input.state);
    if (!row || row.consumed || row.nonce_hash !== input.nonceHash || row.expires < Date.now()) return json({ error: "invalid_transaction" }, 401);
    return json({ verifier: row.verifier });
  }
  private async create(input: Json): Promise<Response> {
    if (!this.env.BROWSER_SESSION_KEY || !string(input.hash, 128) || !string(input.ownerId, 256) || !string(input.providerSessionId, 256) || !string(input.csrfToken, 256) || !string(input.accessToken) || !string(input.refreshToken) || typeof input.providerExpiresAt !== "number" || (input.organizationId !== undefined && !string(input.organizationId, 256))) return json({ error: "invalid_request" }, 400);
    const now = Date.now();
    const absolute = now + MAX_AGE;
    this.ctx.storage.sql.exec(
      "INSERT INTO browser_sessions(hash, owner, sid, csrf, organization_id, access_secret, refresh_secret, provider_expiry, created, last_active, absolute_expiry) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
      input.hash, input.ownerId, input.providerSessionId, input.csrfToken, input.organizationId ?? null,
      await seal(this.env.BROWSER_SESSION_KEY, input.accessToken), await seal(this.env.BROWSER_SESSION_KEY, input.refreshToken), input.providerExpiresAt, now, now, absolute
    );
    await this.schedule();
    return json({ expiresAt: absolute, generation: 1 });
  }
  private async validate(input: Json): Promise<Response> {
    if (!string(input.hash, 128) || typeof input.touch !== "boolean") return json({ error: "invalid_request" }, 400);
    const row = await this.session(input.hash);
    if (!row) return json({ authenticated: false });
    const refreshed = input.refresh === false ? row : await this.refreshIfNeeded(row);
    if (!refreshed) return json({ authenticated: false });
    if (input.touch) this.ctx.storage.sql.exec("UPDATE browser_sessions SET last_active = ? WHERE hash = ? AND generation = ? AND revoked = 0", Date.now(), refreshed.hash, refreshed.generation);
    return json({ authenticated: true, ownerId: refreshed.owner, providerSessionId: refreshed.sid, csrfToken: refreshed.csrf, expiresAt: refreshed.absolute_expiry, generation: refreshed.generation, organizationId: refreshed.organization_id ?? undefined });
  }
  private async activity(input: Json): Promise<Response> {
    if (!string(input.hash, 128) || !string(input.csrf, 256)) return json({ error: "invalid_request" }, 400);
    const row = await this.session(input.hash);
    if (!row || row.csrf !== input.csrf || !(await this.refreshIfNeeded(row))) return json({ authenticated: false });
    this.ctx.storage.sql.exec("UPDATE browser_sessions SET last_active = ? WHERE hash = ?", Date.now(), row.hash);
    return json({ authenticated: true });
  }
  private async list(input: Json): Promise<Response> {
    if (!string(input.ownerId, 256)) return json({ error: "invalid_request" }, 400);
    const sessions = [...this.ctx.storage.sql.exec("SELECT hash, created, last_active, absolute_expiry FROM browser_sessions WHERE owner = ? AND revoked = 0 ORDER BY last_active DESC LIMIT 64", input.ownerId)] as Array<{ hash: string; created: number; last_active: number; absolute_expiry: number }>;
    return json({ sessions });
  }
  private async revoke(input: Json): Promise<Response> {
    if (!string(input.ownerId, 256) || !string(input.hash, 128)) return json({ error: "invalid_request" }, 400);
    const owned = [...this.ctx.storage.sql.exec("SELECT hash, revoked FROM browser_sessions WHERE hash = ? AND owner = ?", input.hash, input.ownerId)][0] as { hash: string; revoked: number } | undefined;
    if (owned) {
      if (!owned.revoked) this.ctx.storage.sql.exec("UPDATE browser_sessions SET revoked = 1, generation = generation + 1 WHERE hash = ?", owned.hash);
      await this.notifyBindings(owned.hash);
    }
    await this.schedule();
    return json({ ok: true });
  }
  private async revokeAll(input: Json): Promise<Response> {
    if (!string(input.ownerId, 256)) return json({ error: "invalid_request" }, 400);
    const hashes = [...this.ctx.storage.sql.exec("SELECT hash, revoked FROM browser_sessions WHERE owner = ?", input.ownerId)] as Array<{ hash: string; revoked: number }>;
    this.ctx.storage.sql.exec("UPDATE browser_sessions SET revoked = 1, generation = generation + 1 WHERE owner = ? AND revoked = 0", input.ownerId);
    await Promise.all(hashes.map(({ hash }) => this.notifyBindings(hash)));
    await this.schedule();
    return json({ ok: true });
  }
  private async registerDevice(input: Json): Promise<Response> {
    const name = typeof input.name === "string" ? input.name.trim() : undefined;
    if (!string(input.ownerId, 256) || !string(input.deviceId, 128) || (input.name !== undefined && !string(name ?? "", 128))) return json({ error: "invalid_request" }, 400);
    this.ctx.storage.sql.exec(
      "INSERT INTO browser_devices(owner, device_id, name, seen_at) VALUES (?, ?, ?, ?) ON CONFLICT(owner, device_id) DO UPDATE SET name = COALESCE(excluded.name, browser_devices.name), seen_at = excluded.seen_at",
      input.ownerId,
      input.deviceId,
      name ?? null,
      Date.now()
    );
    return json({ ok: true });
  }
  private async devices(input: Json): Promise<Response> {
    if (!string(input.ownerId, 256)) return json({ error: "invalid_request" }, 400);
    const devices = [...this.ctx.storage.sql.exec(
      "SELECT device_id, name FROM browser_devices WHERE owner = ? ORDER BY seen_at DESC LIMIT 64",
      input.ownerId
    )] as Array<{ device_id: string; name: string | null }>;
    return json({ devices: devices.map(({ device_id, name }) => ({ id: device_id, ...(name ? { name } : {}) })) });
  }

  private async previewCreate(input: Json): Promise<Response> {
    if (!string(input.hash, 128) || !string(input.parentHash, 128) || !string(input.ownerId, 256) || !string(input.deviceId, 128) || !string(input.serviceId, 128) || !string(input.host, 253)) return json({ error: "invalid_request" }, 400);
    const parent = await this.session(input.parentHash);
    if (!parent || parent.owner !== input.ownerId) return json({ authenticated: false }, 401);
    const expires = Date.now() + 15 * 60_000;
    this.ctx.storage.sql.exec("INSERT INTO preview_sessions(hash, parent_hash, owner, device_id, service_id, host, expires) VALUES (?, ?, ?, ?, ?, ?, ?)", input.hash, input.parentHash, input.ownerId, input.deviceId, input.serviceId, input.host, expires);
    await this.schedule();
    return json({ expiresAt: expires });
  }
  private async previewValidate(input: Json): Promise<Response> {
    if (!string(input.hash, 128)) return json({ error: "invalid_request" }, 400);
    const row = [...this.ctx.storage.sql.exec("SELECT hash, parent_hash, owner, device_id, service_id, host, expires FROM preview_sessions WHERE hash = ?", input.hash)][0] as { hash: string; parent_hash: string; owner: string; device_id: string; service_id: string; host: string | null; expires: number } | undefined;
    if (!row || !string(row.host ?? "", 253) || row.expires <= Date.now() || !(await this.session(row.parent_hash))) {
      if (row) this.ctx.storage.sql.exec("DELETE FROM preview_sessions WHERE hash = ?", row.hash);
      return json({ authenticated: false });
    }
    return json({ authenticated: true, ownerId: row.owner, deviceId: row.device_id, serviceId: row.service_id, host: row.host, expiresAt: row.expires, parentHash: row.parent_hash });
  }



  private async bind(input: Json): Promise<Response> {
    if (!string(input.hash, 128) || !string(input.room, 256) || !string(input.connId, 256)) return json({ error: "invalid_request" }, 400);
    if (!await this.session(input.hash)) return json({ bound: false }, 401);
    this.ctx.storage.sql.exec("INSERT INTO browser_bindings(hash, room, conn_id, next_retry) VALUES (?, ?, ?, ?) ON CONFLICT(hash, room, conn_id) DO UPDATE SET next_retry = excluded.next_retry", input.hash, input.room, input.connId, NO_RETRY);
    return json({ bound: true });
  }
  private async unbind(input: Json): Promise<Response> {
    if (!string(input.hash, 128) || !string(input.room, 256) || !string(input.connId, 256)) return json({ error: "invalid_request" }, 400);
    this.ctx.storage.sql.exec("DELETE FROM browser_bindings WHERE hash = ? AND room = ? AND conn_id = ?", input.hash, input.room, input.connId);
    return json({ ok: true });
  }
  private async notifyBindings(hash: string): Promise<void> {
    const now = Date.now();

    this.ctx.storage.sql.exec("UPDATE browser_bindings SET next_retry = ? WHERE hash = ? AND next_retry = ?", now, hash, NO_RETRY);
    const bindings = [...this.ctx.storage.sql.exec("SELECT room, conn_id FROM browser_bindings WHERE hash = ? AND next_retry <= ?", hash, now)] as Array<{ room: string; conn_id: string }>;
    await Promise.all(bindings.map(async ({ room, conn_id }) => {
      try {
        const response = await this.env.DEVICE_ROOMS.get(this.env.DEVICE_ROOMS.idFromName(room)).fetch(
          new Request("https://device-room/browser-revoke", { method: "POST", headers: { "content-type": "application/json", [SESSION_STORE_HEADER]: "1" }, body: JSON.stringify({ hash, connId: conn_id }) })
        );
        if (response.ok) {
          this.ctx.storage.sql.exec("DELETE FROM browser_bindings WHERE hash = ? AND room = ? AND conn_id = ?", hash, room, conn_id);
          return;
        }
      } catch { /* retain durable delivery for alarm retry */ }
      this.ctx.storage.sql.exec("UPDATE browser_bindings SET next_retry = ? WHERE hash = ? AND room = ? AND conn_id = ?", now + 10_000, hash, room, conn_id);
    }));
  }


  private async session(hash: string): Promise<SessionRow | undefined> {
    const row = [...this.ctx.storage.sql.exec("SELECT * FROM browser_sessions WHERE hash = ?", hash)][0] as SessionRow | undefined;
    if (!row || row.revoked) return undefined;
    if (row.absolute_expiry <= Date.now() || row.last_active + IDLE_AGE <= Date.now()) {
      await this.invalidate(row.hash, row.generation);
      return undefined;
    }
    return row;
  }
  private async invalidate(hash: string, generation?: number): Promise<void> {
    const clause = generation === undefined ? "" : " AND generation = ?";
    const args = generation === undefined ? [hash] : [hash, generation];
    this.ctx.storage.sql.exec(`UPDATE browser_sessions SET revoked = 1, generation = generation + 1 WHERE hash = ? AND revoked = 0${clause}`, ...args);
    await this.notifyBindings(hash);
    await this.schedule();
  }
  private async refreshIfNeeded(row: SessionRow): Promise<SessionRow | undefined> {
    if (row.provider_expiry > Date.now() + REFRESH_EARLY) return row;
    let pending = this.refreshing.get(row.hash);
    if (!pending) {
      pending = this.refreshExpiredSession(row).finally(() => this.refreshing.delete(row.hash));
      this.refreshing.set(row.hash, pending);
    }
    return pending;
  }
  private async refreshExpiredSession(row: SessionRow): Promise<SessionRow | undefined> {
    if (!this.env.WORKOS_API_KEY || !this.env.BROWSER_SESSION_KEY) return undefined;
    // A queued request may have revoked/replaced the record before this refresh starts.
    const before = await this.session(row.hash);
    if (!before || before.generation !== row.generation || before.provider_expiry > Date.now() + REFRESH_EARLY) return before;
    try {
      const refreshed = await refresh(this.env, this.env.WORKOS_API_KEY, await open(this.env.BROWSER_SESSION_KEY, before.refresh_secret));
      const verified = await verifyBrowserToken(this.env, refreshed.accessToken);
      if (!verified?.sessionId || verified.userId !== before.owner || verified.sessionId !== before.sid || !verified.expiresAt) throw new WorkOsAuthFailed();
      // Re-read after the network await: logout/revoke/idle expiry wins over a
      // late rotated token and can never resurrect a session.
      const after = await this.session(before.hash);
      if (!after || after.generation !== before.generation || after.revoked) return undefined;
      this.ctx.storage.sql.exec(
        "UPDATE browser_sessions SET access_secret = ?, refresh_secret = ?, provider_expiry = ? WHERE hash = ? AND generation = ? AND revoked = 0 AND absolute_expiry > ? AND last_active + ? > ?",
        await seal(this.env.BROWSER_SESSION_KEY, refreshed.accessToken), await seal(this.env.BROWSER_SESSION_KEY, refreshed.refreshToken), verified.expiresAt,
        after.hash, after.generation, Date.now(), IDLE_AGE, Date.now()
      );
      const updated = await this.session(after.hash);
      return updated && updated.generation === after.generation ? updated : undefined;
    } catch (error) {
      if (error instanceof WorkOsAuthFailed && error.kind === "invalid") await this.invalidate(row.hash, row.generation);
      return undefined;
    }
  }
  async alarm(): Promise<void> {
    this.ctx.storage.sql.exec("DELETE FROM preview_sessions WHERE expires <= ?", Date.now());

    const now = Date.now();
    this.ctx.storage.sql.exec("DELETE FROM browser_transactions WHERE expires <= ?", now);
    const expired = [...this.ctx.storage.sql.exec("SELECT hash, generation FROM browser_sessions WHERE revoked = 0 AND (absolute_expiry <= ? OR last_active + ? <= ?)", now, IDLE_AGE, now)] as Array<{ hash: string; generation: number }>;
    const pending = [...this.ctx.storage.sql.exec("SELECT DISTINCT hash FROM browser_bindings WHERE next_retry <= ?", now)] as Array<{ hash: string }>;
    await Promise.all([...expired.map(({ hash, generation }) => this.invalidate(hash, generation)), ...pending.map(({ hash }) => this.notifyBindings(hash))]);
    await this.schedule();
  }
  private async schedule(): Promise<void> {
    const row = [...this.ctx.storage.sql.exec("SELECT MIN(deadline) AS deadline FROM (SELECT expires AS deadline FROM browser_transactions UNION ALL SELECT MIN(absolute_expiry, last_active + ?) AS deadline FROM browser_sessions WHERE revoked = 0 UNION ALL SELECT MIN(next_retry) AS deadline FROM browser_bindings WHERE next_retry < ? UNION ALL SELECT MIN(expires) AS deadline FROM preview_sessions)", IDLE_AGE, NO_RETRY)][0] as { deadline?: unknown } | undefined;
    if (typeof row?.deadline === "number" && Number.isFinite(row.deadline)) await this.ctx.storage.setAlarm(Math.max(Date.now(), row.deadline));
    else await this.ctx.storage.deleteAlarm();
  }
}

type SessionRow = { hash: string; owner: string; sid: string; csrf: string; organization_id?: string | null; access_secret: string; refresh_secret: string; provider_expiry: number; created: number; last_active: number; absolute_expiry: number; generation: number; revoked: number };

const store = (env: Env) => env.BROWSER_SESSIONS.get(env.BROWSER_SESSIONS.idFromName(SESSION_STORE_NAME));
const invoke = async (env: Env, path: string, value: Json): Promise<Json | undefined> => {
  const response = await store(env).fetch(new Request(`https://browser-sessions${path}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(value) }));
  return response.ok ? (await response.json()) as Json : undefined;
};
export const createBrowserTransaction = (env: Env, value: Json) => invoke(env, "/transaction", value);
export const consumeBrowserTransaction = (env: Env, value: Json) => invoke(env, "/consume-transaction", value);
export const createBrowserSession = (env: Env, value: Json) => invoke(env, "/create", value);

export const bindBrowserSession = (env: Env, hash: string, room: string, connId: string) => invoke(env, "/bind", { hash, room, connId });
export const unbindBrowserSession = (env: Env, hash: string, room: string, connId: string) => invoke(env, "/unbind", { hash, room, connId });

export const registerBrowserDevice = (env: Env, ownerId: string, deviceId: string, name?: string) => invoke(env, "/register-device", { ownerId, deviceId, ...(name ? { name } : {}) });
export const listBrowserDevices = (env: Env, ownerId: string) => invoke(env, "/devices", { ownerId });
export const validateBrowserSession = async (
  env: Env,
  hash: string,
  touch = false,
  refreshProvider = true
): Promise<BrowserSession | undefined> => {
  const value = await invoke(env, "/validate", { hash, touch, refresh: refreshProvider });
  if (!value || value.authenticated !== true || !string(value.ownerId, 256) || !string(value.providerSessionId, 256) || !string(value.csrfToken, 256) || typeof value.expiresAt !== "number" || typeof value.generation !== "number") return undefined;
  return {
    hash,
    ownerId: value.ownerId,
    providerSessionId: value.providerSessionId,
    csrfToken: value.csrfToken,
    expiresAt: value.expiresAt,
    generation: value.generation,
    ...(typeof value.organizationId === "string" ? { organizationId: value.organizationId } : {})
  };
};
export const browserActivity = (env: Env, hash: string, csrf: string) => invoke(env, "/activity", { hash, csrf });
export const listBrowserSessions = (env: Env, ownerId: string) => invoke(env, "/list", { ownerId });
export const revokeBrowserSession = (env: Env, ownerId: string, hash: string) => invoke(env, "/revoke", { ownerId, hash });
export const revokeAllBrowserSessions = (env: Env, ownerId: string) => invoke(env, "/revoke-all", { ownerId });
export const createPreviewSession = (env: Env, hash: string, parentHash: string, ownerId: string, deviceId: string, serviceId: string, host: string) => invoke(env, "/preview-create", { hash, parentHash, ownerId, deviceId, serviceId, host });
export const validatePreviewSession = async (env: Env, hash: string): Promise<PreviewSession | undefined> => {
  const value = await invoke(env, "/preview-validate", { hash });
  if (!value || value.authenticated !== true || !string(value.parentHash, 128) || !string(value.ownerId, 256) || !string(value.deviceId, 128) || !string(value.serviceId, 128) || !string(value.host, 253) || typeof value.expiresAt !== "number") return undefined;
  return { hash, parentHash: value.parentHash, ownerId: value.ownerId, deviceId: value.deviceId, serviceId: value.serviceId, host: value.host, expiresAt: value.expiresAt };
};
