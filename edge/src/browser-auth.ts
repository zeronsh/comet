import { verifyBrowserToken } from "./auth";
import type { Env } from "./env";
import { WorkOsAuthFailed, exchangeWithVerifier, refresh, revokeSession } from "./workos";

/**
 * Private browser-BFF API. The public browser never calls these routes: its
 * owner-hosted gateway uses the broker token and keeps the returned provider
 * credentials in its encrypted session store.
 */
export type BrowserBrokerEnv = Env & {
  readonly BROWSER_BROKER_TOKEN?: string;
};

export interface BrowserProviderTokens {
  readonly accessToken: string;
  readonly refreshToken: string;
  readonly sessionId: string;
}

export interface BrowserExchangeResponse {
  readonly user: {
    readonly id: string;
    readonly email: string;
    readonly firstName: string | null;
    readonly lastName: string | null;
  };
  readonly provider: BrowserProviderTokens;
  /** Server-only, signed proof that this sid came from this browser exchange. */
  readonly browserSessionGrant: string;
}

export interface BrowserRefreshResponse {
  readonly provider: BrowserProviderTokens;
}

const MAX_JSON_BYTES = 8 * 1024;
const MAX_CODE_BYTES = 4096;
const MAX_TOKEN_BYTES = 4096;
const MAX_GRANT_BYTES = 1024;
const GRANT_LIFETIME_SECONDS = 30 * 24 * 60 * 60;
const PKCE_VERIFIER = /^[A-Za-z0-9._~-]{43,128}$/;
const SESSION_ID = /^[A-Za-z0-9_-]{1,256}$/;
const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

type Grant = {
  readonly v: 1;
  readonly sub: string;
  readonly sid: string;
  readonly exp: number;
};

type JsonRecord = Record<string, unknown>;

const response = (body: unknown, status = 200, headers?: HeadersInit): Response =>
  new Response(JSON.stringify(body), {
    status,
    headers: {
      "cache-control": "no-store",
      "content-type": "application/json",
      ...headers
    }
  });

const invalidRequest = (): Response => response({ error: "invalid_request" }, 400);
const unauthorized = (): Response => response({ error: "unauthorized" }, 401);
const invalidCredentials = (): Response => response({ error: "invalid_credentials" }, 401);
const notConfigured = (): Response => response({ error: "browser_broker_not_configured" }, 501);

const isRecord = (value: unknown): value is JsonRecord =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const hasOnlyKeys = (body: JsonRecord, keys: readonly string[]): boolean =>
  Object.keys(body).every((key) => keys.includes(key)) && keys.every((key) => key in body);

const boundedString = (value: unknown, max: number): value is string =>
  typeof value === "string" && value.length > 0 && value.length <= max;

/** Read no more than the broker's small DTO budget, including chunked bodies. */
const readJson = async (request: Request): Promise<JsonRecord | undefined> => {
  const declaredLength = request.headers.get("content-length");
  if (declaredLength !== null) {
    const length = Number(declaredLength);
    if (!Number.isSafeInteger(length) || length < 0 || length > MAX_JSON_BYTES) return undefined;
  }
  if (!request.body) return undefined;
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      length += chunk.value.byteLength;
      if (length > MAX_JSON_BYTES) {
        await reader.cancel();
        return undefined;
      }
      chunks.push(chunk.value);
    }
  } catch {
    return undefined;
  }
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  try {
    const value: unknown = JSON.parse(textDecoder.decode(bytes));
    return isRecord(value) ? value : undefined;
  } catch {
    return undefined;
  }
};

const base64Url = (bytes: Uint8Array): string => {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
};

const fromBase64Url = (value: string): Uint8Array | undefined => {
  if (!/^[A-Za-z0-9_-]+$/.test(value)) return undefined;
  try {
    const binary = atob(value.replaceAll("-", "+").replaceAll("_", "/"));
    return Uint8Array.from(binary, (character) => character.charCodeAt(0));
  } catch {
    return undefined;
  }
};

const grantKey = (brokerToken: string): Promise<CryptoKey> =>
  crypto.subtle.importKey("raw", textEncoder.encode(brokerToken), { name: "HMAC", hash: "SHA-256" }, false, [
    "sign",
    "verify"
  ]);

const makeGrant = async (brokerToken: string, sub: string, sid: string): Promise<string> => {
  const payload = base64Url(
    textEncoder.encode(JSON.stringify({ v: 1, sub, sid, exp: Math.floor(Date.now() / 1000) + GRANT_LIFETIME_SECONDS }))
  );
  const signature = new Uint8Array(await crypto.subtle.sign("HMAC", await grantKey(brokerToken), textEncoder.encode(payload)));
  return `${payload}.${base64Url(signature)}`;
};

const readGrant = async (brokerToken: string, value: unknown): Promise<Grant | undefined> => {
  if (!boundedString(value, MAX_GRANT_BYTES)) return undefined;
  const parts = value.split(".");
  if (parts.length !== 2) return undefined;
  const [payload, signature] = parts;
  const decoded = fromBase64Url(payload);
  const signatureBytes = fromBase64Url(signature);
  if (!decoded || !signatureBytes) return undefined;
  const valid = await crypto.subtle.verify("HMAC", await grantKey(brokerToken), signatureBytes, textEncoder.encode(payload));
  if (!valid) return undefined;
  try {
    const grant: unknown = JSON.parse(textDecoder.decode(decoded));
    if (!isRecord(grant) || !hasOnlyKeys(grant, ["v", "sub", "sid", "exp"])) return undefined;
    if (
      grant.v !== 1 ||
      !boundedString(grant.sub, 256) ||
      !SESSION_ID.test(grant.sid as string) ||
      typeof grant.exp !== "number" ||
      !Number.isSafeInteger(grant.exp) ||
      grant.exp < Math.floor(Date.now() / 1000) ||
      grant.exp > Math.floor(Date.now() / 1000) + GRANT_LIFETIME_SECONDS + 60
    ) {
      return undefined;
    }
    return grant as Grant;
  } catch {
    return undefined;
  }
};

const workOsFailure = (error: unknown): Response => {
  if (!(error instanceof WorkOsAuthFailed)) return response({ error: "upstream_unavailable" }, 503);
  if (error.kind === "rate_limited") {
    return response(
      { error: "upstream_rate_limited" },
      429,
      error.retryAfterSeconds ? { "retry-after": String(error.retryAfterSeconds) } : undefined
    );
  }
  if (error.kind === "unavailable") return response({ error: "upstream_unavailable" }, 503);
  return invalidCredentials();
};

const configured = (env: BrowserBrokerEnv): env is BrowserBrokerEnv & { readonly WORKOS_API_KEY: string; readonly BROWSER_BROKER_TOKEN: string } =>
  Boolean(env.WORKOS_API_KEY && env.BROWSER_BROKER_TOKEN && env.WORKOS_ISSUER && env.WORKOS_JWKS_URL);

const authorized = (request: Request, brokerToken: string): boolean =>
  request.headers.get("x-comet-broker-token") === brokerToken;

/** Handle only the fixed browser BFF routes under /auth/browser. */
export const handleBrowserAuthRoute = async (
  request: Request,
  env: BrowserBrokerEnv,
  url: URL
): Promise<Response> => {
  if (!configured(env)) return notConfigured();
  if (!authorized(request, env.BROWSER_BROKER_TOKEN)) return unauthorized();
  const parts = url.pathname.split("/").filter(Boolean);
  if (request.method !== "POST" || parts.length !== 3) return response({ error: "not_found" }, 404);
  const body = await readJson(request);
  if (!body) return invalidRequest();

  if (parts[2] === "exchange") {
    if (!hasOnlyKeys(body, ["code", "codeVerifier"]) || !boundedString(body.code, MAX_CODE_BYTES) || !PKCE_VERIFIER.test(body.codeVerifier as string)) {
      return invalidRequest();
    }
    try {
      const exchanged = await exchangeWithVerifier(env, env.WORKOS_API_KEY, body.code as string, body.codeVerifier as string);
      const verified = await verifyBrowserToken(env, exchanged.accessToken);
      if (!verified?.sessionId || exchanged.user.id !== verified.userId) return invalidCredentials();
      const grant = await makeGrant(env.BROWSER_BROKER_TOKEN, verified.userId, verified.sessionId);
      return response({
        user: exchanged.user,
        provider: {
          accessToken: exchanged.accessToken,
          refreshToken: exchanged.refreshToken,
          sessionId: verified.sessionId
        },
        browserSessionGrant: grant
      } satisfies BrowserExchangeResponse);
    } catch (error) {
      return workOsFailure(error);
    }
  }

  if (parts[2] === "refresh") {
    if (
      !hasOnlyKeys(body, ["refreshToken", "browserSessionGrant"]) ||
      !boundedString(body.refreshToken, MAX_TOKEN_BYTES)
    ) {
      return invalidRequest();
    }
    const grant = await readGrant(env.BROWSER_BROKER_TOKEN, body.browserSessionGrant);
    if (!grant) return invalidCredentials();
    try {
      const refreshed = await refresh(env, env.WORKOS_API_KEY, body.refreshToken as string);
      const verified = await verifyBrowserToken(env, refreshed.accessToken);
      if (!verified?.sessionId || verified.userId !== grant.sub || verified.sessionId !== grant.sid) return invalidCredentials();
      return response({
        provider: {
          accessToken: refreshed.accessToken,
          refreshToken: refreshed.refreshToken,
          sessionId: verified.sessionId
        }
      } satisfies BrowserRefreshResponse);
    } catch (error) {
      return workOsFailure(error);
    }
  }

  if (parts[2] === "revoke") {
    if (!hasOnlyKeys(body, ["browserSessionGrant"])) return invalidRequest();
    const grant = await readGrant(env.BROWSER_BROKER_TOKEN, body.browserSessionGrant);
    if (!grant) return invalidCredentials();
    try {
      await revokeSession(env.WORKOS_API_KEY, grant.sid);
      return response({ revoked: true });
    } catch (error) {
      return workOsFailure(error);
    }
  }

  return response({ error: "not_found" }, 404);
};
