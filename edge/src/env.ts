export interface Env {
  SESSION_ROOMS: DurableObjectNamespace;
  DEVICE_ROOMS: DurableObjectNamespace;
  /** Single durable authority for browser login transactions and sessions. */
  BROWSER_SESSIONS: DurableObjectNamespace;
  PREVIEW_ROOMS: DurableObjectNamespace;
  /** Per-user workspace registries (`reg1/{orgId}/{userId}`) — the row-table
   * replacement for the Loro workspace doc (docs/registry-sync.md). */
  REGISTRY_ROOMS: DurableObjectNamespace;
  /** chat2 session rooms (`chat2/{chatId}`) — dumb authenticated log relays
   * replacing SessionRoom's loro-aware s2 rooms (docs/chat2-sync.md). */
  CHAT_ROOMS: DurableObjectNamespace;
  BLOBS: R2Bucket;
  /** Release artifacts (headless tarballs, dmgs, latest.txt) served at
   * /releases/* for the curl-install flow. */
  RELEASES: R2Bucket;
  WORKOS_CLIENT_ID: string;
  /** "workos" (verify AuthKit JWTs) or "dev" (bearer == userId, never prod). */
  AUTH_MODE: string;
  /** Optional overrides for the WorkOS trust anchor. */
  WORKOS_ISSUER?: string;
  WORKOS_JWKS_URL?: string;
  /** WorkOS secret API key (wrangler secret) — powers the absorbed /auth/*
   * routes (code exchange, refresh, orgs). Unset ⇒ those routes answer 501,
   * matching the old apps/server dev-mode behavior. */
  WORKOS_API_KEY?: string;
  /** Browser BFF deployment inputs; unset makes only browser auth unavailable. */
  WORKOS_BROWSER_ORIGIN?: string;
  WORKOS_BROWSER_OWNER_SUBJECT?: string;
  /** Dedicated secret used to encrypt recoverable provider refresh credentials. */
  BROWSER_SESSION_KEY?: string;

  /** Dedicated origin that serves untrusted remote preview bytes, never the app BFF. */
  BROWSER_PREVIEW_ORIGIN?: string;
  /** Explicit loopback-only owner for local browser/engine integration tests. */
  BROWSER_DEV_OWNER_SUBJECT?: string;
  BROWSER_DEV_ORGANIZATION_ID?: string;

  /** Fixed loopback origin accepted by the local browser proxy. */
  BROWSER_DEV_ORIGIN?: string;

  /** Ephemeral capability shared only by the loopback development proxy and worker. */
  BROWSER_DEV_PROXY_KEY?: string;
}

/** Header the Worker stamps on requests it forwards into DOs after verifying
 * the caller's JWT. DOs trust it blindly — they are only reachable through
 * the Worker (design §2: "DO never sees an unauthenticated frame"). */
export const AUTH_USER_HEADER = "x-zeron-auth-user";

/** Header the Worker stamps on requests forwarded into workspace-doc rooms
 * (`ws/{orgId}`). Membership (JWT org claim == orgId) is enforced at the
 * Worker; the SessionRoom DO sees this and skips its per-chat
 * claim-on-first-join ownership discipline for the room. */
export const ROOM_KIND_HEADER = "x-zeron-room-kind";
