import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { SignJWT, exportJWK, generateKeyPair } from "jose";
import { handleBrowserAuthRoute, type BrowserBrokerEnv } from "./browser-auth";

const ISSUER = "https://issuer.example/authkit";
const JWKS_URL = "https://issuer.example/jwks";
const BROKER_TOKEN = "broker-token-for-test-only-which-is-long-enough";
const PROVIDER_URL = "https://api.workos.com/user_management/authenticate";

const REVOKE_URL = "https://api.workos.com/user_management/sessions/revoke";

let signingKey: CryptoKey;
let publicJwk: { kid?: string };
let upstream: Array<Response | Error>;
let requests: Array<{ readonly url: string; readonly init?: RequestInit }>;

const env = (): BrowserBrokerEnv =>
  ({
    AUTH_MODE: "dev",
    WORKOS_CLIENT_ID: "client_browser",
    WORKOS_API_KEY: "workos-secret-not-real",
    WORKOS_ISSUER: ISSUER,
    WORKOS_JWKS_URL: JWKS_URL,
    BROWSER_BROKER_TOKEN: BROKER_TOKEN
  }) as BrowserBrokerEnv;

const accessToken = async (sub = "owner_1", sid = "session_browser_1", issuer = ISSUER): Promise<string> =>
  new SignJWT({ sid, auth_time: Math.floor(Date.now() / 1000) })
    .setProtectedHeader({ alg: "RS256", kid: "browser-test" })
    .setIssuer(issuer)
    .setSubject(sub)
    .setIssuedAt()
    .setExpirationTime("5m")
    .sign(signingKey);

const providerReply = (token: string, status = 200): Response =>
  new Response(
    JSON.stringify(
      status === 200
        ? {
            user: { id: "owner_1", email: "owner@example.test", first_name: "Owner", last_name: null },
            access_token: token,
            refresh_token: "rotated-refresh-token"
          }
        : { error: "provider detail that must not reach the gateway" }
    ),
    { status, headers: status === 429 ? { "retry-after": "12" } : undefined }
  );

const call = (path: string, body: unknown, brokerToken = BROKER_TOKEN): Promise<Response> =>
  handleBrowserAuthRoute(
    new Request(`https://edge.example${path}`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-comet-broker-token": brokerToken },
      body: JSON.stringify(body)
    }),
    env(),
    new URL(`https://edge.example${path}`)
  );

beforeAll(async () => {
  const pair = await generateKeyPair("RS256");
  signingKey = pair.privateKey;
  publicJwk = await exportJWK(pair.publicKey);
  publicJwk.kid = "browser-test";
});

beforeEach(() => {
  upstream = [];
  requests = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      const request = input instanceof Request ? input : undefined;
      const url = request ? request.url : input.toString();
      if (url === JWKS_URL) return new Response(JSON.stringify({ keys: [publicJwk] }));
      if (url !== PROVIDER_URL && url !== REVOKE_URL) throw new Error(`unexpected upstream ${url}`);
      requests.push({ url, init: init ?? (request ? { method: request.method, body: request.body } : undefined) });
      const reply = upstream.shift();
      if (!reply) throw new Error("no mocked WorkOS response");
      if (reply instanceof Error) throw reply;
      return reply;
    })
  );
});

afterAll(() => vi.unstubAllGlobals());

describe("browser WorkOS broker", () => {
  it("requires a bounded S256 verifier and forwards it only on the browser exchange", async () => {
    upstream.push(providerReply(await accessToken()));

    const response = await call("/auth/browser/exchange", {
      code: "authorization-code",
      codeVerifier: "a".repeat(43)
    });

    expect(response.status).toBe(200);
    expect(response.headers.get("cache-control")).toBe("no-store");
    const result = (await response.json()) as { provider: { sessionId: string }; browserSessionGrant: string };
    expect(result.provider.sessionId).toBe("session_browser_1");
    expect(result.browserSessionGrant).toContain(".");
    expect(requests).toHaveLength(1);
    expect(JSON.parse(requests[0].init?.body as string)).toEqual({
      client_id: "client_browser",
      client_secret: "workos-secret-not-real",
      grant_type: "authorization_code",
      code: "authorization-code",
      code_verifier: "a".repeat(43)
    });
  });

  it("preserves the exchange-issued browser grant through refresh and rejects an arbitrary revoke sid", async () => {
    upstream.push(providerReply(await accessToken()));
    const exchange = await call("/auth/browser/exchange", { code: "code", codeVerifier: "b".repeat(43) });
    const exchanged = (await exchange.json()) as { browserSessionGrant: string };

    upstream.push(providerReply(await accessToken()));
    const refreshed = await call("/auth/browser/refresh", {
      refreshToken: "old-refresh-token",
      browserSessionGrant: exchanged.browserSessionGrant
    });
    expect(refreshed.status).toBe(200);
    expect(JSON.parse(requests[1].init?.body as string)).toEqual({
      client_id: "client_browser",
      client_secret: "workos-secret-not-real",
      grant_type: "refresh_token",
      refresh_token: "old-refresh-token"
    });

    const blocked = await call("/auth/browser/revoke", { browserSessionGrant: "session_native_999" });
    expect(blocked.status).toBe(401);
    expect(await blocked.json()).toEqual({ error: "invalid_credentials" });
    expect(requests).toHaveLength(2);

    upstream.push(new Response(null, { status: 204 }));
    const revoked = await call("/auth/browser/revoke", { browserSessionGrant: exchanged.browserSessionGrant });
    expect(revoked.status).toBe(200);
    expect(JSON.parse(requests[2].init?.body as string)).toEqual({ session_id: "session_browser_1" });
  });

  it("fails closed for missing broker authorization, malformed DTOs, and mismatched identities", async () => {
    const unauthorized = await call(
      "/auth/browser/exchange",
      { code: "code", codeVerifier: "c".repeat(43) },
      "wrong-broker-token"
    );
    expect(unauthorized.status).toBe(401);
    expect(requests).toHaveLength(0);

    const malformed = await call("/auth/browser/exchange", { code: "code", codeVerifier: "too-short", sid: "session_1" });
    expect(malformed.status).toBe(400);
    expect(await malformed.json()).toEqual({ error: "invalid_request" });

    upstream.push(providerReply(await accessToken("another_owner")));
    const mismatch = await call("/auth/browser/exchange", { code: "code", codeVerifier: "c".repeat(43) });
    expect(mismatch.status).toBe(401);
    expect(await mismatch.json()).toEqual({ error: "invalid_credentials" });
  });

  it("returns sanitized structured transient errors without logging credential material", async () => {
    upstream.push(providerReply("unused", 429));
    const response = await call("/auth/browser/exchange", { code: "sensitive-code", codeVerifier: "d".repeat(43) });

    expect(response.status).toBe(429);
    expect(response.headers.get("retry-after")).toBe("12");
    expect(await response.json()).toEqual({ error: "upstream_rate_limited" });
  });
});
