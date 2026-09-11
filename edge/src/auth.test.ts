import { describe, expect, it } from "vitest";
import { authMode, authenticate, selfhostIdentity, verifyToken } from "./auth";
import type { Env } from "./env";

const env = (vars: Partial<Env>): Env => ({ WORKOS_CLIENT_ID: "", AUTH_MODE: "workos", ...vars }) as Env;

describe("authMode", () => {
  it("normalizes case and whitespace", () => {
    expect(authMode(env({ AUTH_MODE: " None " }))).toBe("none");
    expect(authMode(env({ AUTH_MODE: "DEV" }))).toBe("dev");
  });

  it("treats anything unrecognized as workos", () => {
    expect(authMode(env({ AUTH_MODE: "" }))).toBe("workos");
    expect(authMode(env({ AUTH_MODE: "pair" }))).toBe("workos");
    expect(authMode(env({ AUTH_MODE: undefined as unknown as string }))).toBe("workos");
  });
});

describe("AUTH_MODE=none", () => {
  const open = env({ AUTH_MODE: "none" });

  it("defaults the identity to local/local", () => {
    expect(selfhostIdentity(open)).toEqual({ userId: "local", orgId: "local" });
  });

  it("honours SELFHOST_* overrides and ignores blanks", () => {
    expect(selfhostIdentity(env({ AUTH_MODE: "none", SELFHOST_USER_ID: " dan ", SELFHOST_ORG_ID: "home" }))).toEqual({
      userId: "dan",
      orgId: "home"
    });
    expect(selfhostIdentity(env({ AUTH_MODE: "none", SELFHOST_USER_ID: "  " }))).toEqual({
      userId: "local",
      orgId: "local"
    });
  });

  it("authenticates a request with no bearer at all", async () => {
    const request = new Request("https://edge.local/session/abc/ws");
    expect(await authenticate(open, request)).toEqual({ userId: "local", orgId: "local" });
  });

  it("ignores whatever bearer is presented", async () => {
    expect(await verifyToken(open, "someone@else")).toEqual({ userId: "local", orgId: "local" });
    const request = new Request("https://edge.local/tail/abc", {
      headers: { authorization: "Bearer not-a-jwt" }
    });
    expect(await authenticate(open, request)).toEqual({ userId: "local", orgId: "local" });
  });
});

describe("AUTH_MODE=dev", () => {
  const dev = env({ AUTH_MODE: "dev" });

  it("still requires a bearer", async () => {
    expect(await authenticate(dev, new Request("https://edge.local/tail/abc"))).toBeUndefined();
  });

  it("parses user@org", async () => {
    expect(await verifyToken(dev, "alice@acme")).toEqual({ userId: "alice", orgId: "acme" });
    expect(await verifyToken(dev, "alice")).toEqual({ userId: "alice" });
  });
});
