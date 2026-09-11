/**
 * Minimal WorkOS User Management REST client — the fetch-based port of the
 * old apps/server `WorkOsAuth` service (which used @workos-inc/node; the
 * Worker keeps it SDK-free). This is the one place that holds the WorkOS
 * **API key** (a Worker secret). Device backends build the public authorize
 * URL themselves and delegate the secret-bearing steps here, so the key never
 * lands on a device.
 *
 * Without WORKOS_API_KEY configured the routes answer 501; in dev mode
 * backends use their userId as the bearer and never call these.
 */
import type { Env } from "./env";

const API = "https://api.workos.com";

/** A classified WorkOS failure. Native routes preserve their legacy 401 mapping;
 * browser routes expose only the classification, never provider error text. */
export class WorkOsAuthFailed extends Error {
  constructor(
    message = "authentication failed",
    readonly kind: "invalid" | "rate_limited" | "unavailable" = "invalid",
    readonly retryAfterSeconds?: number
  ) {
    super(message);
  }
}

export interface ExchangeResult {
  readonly user: {
    readonly id: string;
    readonly email: string;
    readonly firstName: string | null;
    readonly lastName: string | null;
  };
  readonly accessToken: string;
  readonly refreshToken: string;
}

export interface RefreshResult {
  readonly accessToken: string;
  readonly refreshToken: string;
}

export interface OrgMembership {
  readonly id: string;
  readonly organizationId: string;
  readonly name: string;
}

interface WireUser {
  id: string;
  email: string;
  first_name: string | null;
  last_name: string | null;
}

interface WireAuthResponse {
  user: WireUser;
  access_token: string;
  refresh_token: string;
}

interface WireMembership {
  id: string;
  organization_id: string;
  organization_name?: string | null;
}

const failed = async (res: Response): Promise<never> => {
  let message = "authentication failed";
  try {
    const body = (await res.json()) as { message?: string; error_description?: string; error?: string };
    message = body.message ?? body.error_description ?? body.error ?? message;
  } catch {
    /* non-JSON error body */
  }
  const retryAfter = Number(res.headers.get("retry-after"));
  const retryAfterSeconds =
    Number.isFinite(retryAfter) && retryAfter > 0 ? Math.min(Math.ceil(retryAfter), 60) : undefined;
  const kind = res.status === 429 ? "rate_limited" : res.status >= 500 ? "unavailable" : "invalid";
  throw new WorkOsAuthFailed(message, kind, retryAfterSeconds);
};

const post = async (apiKey: string, path: string, body: unknown): Promise<Response> =>
  fetch(`${API}${path}`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${apiKey}`,
      "content-type": "application/json"
    },
    body: JSON.stringify(body)
  });

/** `authenticateWithCode`: WorkOS code → tokens + user. */
export const exchange = async (env: Env, apiKey: string, code: string): Promise<ExchangeResult> => {
  const res = await fetch(`${API}/user_management/authenticate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      client_id: env.WORKOS_CLIENT_ID,
      client_secret: apiKey,
      grant_type: "authorization_code",
      code
    })
  });
  if (!res.ok) return failed(res);
  const r = (await res.json()) as WireAuthResponse;
  return {
    user: {
      id: r.user.id,
      email: r.user.email,
      firstName: r.user.first_name,
      lastName: r.user.last_name
    },
    accessToken: r.access_token,
    refreshToken: r.refresh_token
  };
};


/** Browser-only code exchange. Native code-only sign-in intentionally remains
 * on `exchange`; callers must supply an RFC 7636 S256 verifier here. */
export const exchangeWithVerifier = async (
  env: Env,
  apiKey: string,
  code: string,
  codeVerifier: string
): Promise<ExchangeResult> => {
  const res = await fetch(`${API}/user_management/authenticate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      client_id: env.WORKOS_CLIENT_ID,
      client_secret: apiKey,
      grant_type: "authorization_code",
      code,
      code_verifier: codeVerifier
    })
  });
  if (!res.ok) return failed(res);
  const r = (await res.json()) as WireAuthResponse;
  return {
    user: {
      id: r.user.id,
      email: r.user.email,
      firstName: r.user.first_name,
      lastName: r.user.last_name
    },
    accessToken: r.access_token,
    refreshToken: r.refresh_token
  };
};

/** `authenticateWithRefreshToken`; passing `organizationId` scopes the session
 * to that org (the next access token carries `org_id`). */
export const refresh = async (
  env: Env,
  apiKey: string,
  refreshToken: string,
  organizationId?: string
): Promise<RefreshResult> => {
  const res = await fetch(`${API}/user_management/authenticate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      client_id: env.WORKOS_CLIENT_ID,
      client_secret: apiKey,
      grant_type: "refresh_token",
      refresh_token: refreshToken,
      ...(organizationId ? { organization_id: organizationId } : {})
    })
  });
  if (!res.ok) return failed(res);
  const r = (await res.json()) as WireAuthResponse;
  return { accessToken: r.access_token, refreshToken: r.refresh_token };
};


/** End one provider session. Browser routes never accept an arbitrary session
 * id; their caller must present a broker-issued grant bound to this value. */
export const revokeSession = async (apiKey: string, sessionId: string): Promise<void> => {
  const res = await post(apiKey, "/user_management/sessions/revoke", { session_id: sessionId });
  if (!res.ok) return failed(res);
};

/** The user's active organization memberships. */
export const listOrgs = async (apiKey: string, userId: string): Promise<OrgMembership[]> => {
  const params = new URLSearchParams({ user_id: userId, statuses: "active", limit: "100" });
  const res = await fetch(`${API}/user_management/organization_memberships?${params}`, {
    headers: { authorization: `Bearer ${apiKey}` }
  });
  if (!res.ok) return failed(res);
  const r = (await res.json()) as { data: WireMembership[] };
  return r.data.map((m) => ({
    id: m.id,
    organizationId: m.organization_id,
    name: m.organization_name ?? m.organization_id
  }));
};

/** Create an organization and make the user its first (admin) member. */
export const createOrg = async (
  apiKey: string,
  userId: string,
  name: string
): Promise<{ organizationId: string }> => {
  const orgRes = await post(apiKey, "/organizations", { name });
  if (!orgRes.ok) return failed(orgRes);
  const org = (await orgRes.json()) as { id: string };
  // The creator administers their workspace. Role slugs are per-environment
  // config, so fall back to the default role if "admin" doesn't exist rather
  // than failing the whole onboarding.
  const withRole = await post(apiKey, "/user_management/organization_memberships", {
    user_id: userId,
    organization_id: org.id,
    role_slug: "admin"
  });
  if (!withRole.ok) {
    const fallback = await post(apiKey, "/user_management/organization_memberships", {
      user_id: userId,
      organization_id: org.id
    });
    if (!fallback.ok) return failed(fallback);
  }
  return { organizationId: org.id };
};
