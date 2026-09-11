import { SELF, env } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { tokenHash } from "../../src/browser-sessions";
import { AUTH_USER_HEADER } from "../../src/env";

const store = () => env.BROWSER_SESSIONS.get(env.BROWSER_SESSIONS.idFromName("browser-sessions-v1"));
const post = (path: string, body: unknown) =>
  store().fetch(new Request(`https://store${path}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) }));

const host = (id: string, owner: string) =>
  env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(`d2/${id}`)).fetch(
    new Request(`https://device/ws?role=host&connId=host-${id}`, { headers: { upgrade: "websocket", [AUTH_USER_HEADER]: owner } })
  );

describe("browser device discovery", () => {
  it("lists only owner-registered live devices through the production browser API", async () => {
    const owner = `owner-${crypto.randomUUID()}`;
    const device = `device-${crypto.randomUUID()}`;
    const raw = `cookie-${crypto.randomUUID()}`;
    const hash = await tokenHash(raw);
    await post("/create", {
      hash,
      ownerId: owner,
      providerSessionId: "provider",
      csrfToken: "csrf",
      accessToken: "access",
      refreshToken: "refresh",
      providerExpiresAt: Date.now() + 60 * 60_000
    });
    await post("/register-device", { ownerId: owner, deviceId: device });
    const named = `named-${crypto.randomUUID()}`;
    await post("/register-device", { ownerId: owner, deviceId: named, name: "Studio Mac" });
    const connected = await host(device, owner);
    connected.webSocket!.accept();
    const namedConnected = await host(named, owner);
    namedConnected.webSocket!.accept();

    const response = await SELF.fetch("https://test/api/browser/devices", {
      headers: { cookie: `__Host-comet_session=${raw}` }
    });
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      devices: [
        { id: named, name: "Studio Mac", online: true },
        { id: device, online: true }
      ]
    });

    const otherRaw = `cookie-${crypto.randomUUID()}`;
    await post("/create", {
      hash: await tokenHash(otherRaw),
      ownerId: `other-${crypto.randomUUID()}`,
      providerSessionId: "provider",
      csrfToken: "csrf",
      accessToken: "access",
      refreshToken: "refresh",
      providerExpiresAt: Date.now() + 60 * 60_000
    });
    const other = await SELF.fetch("https://test/api/browser/devices", {
      headers: { cookie: `__Host-comet_session=${otherRaw}` }
    });
    expect(await other.json()).toEqual({ devices: [] });
  });

  it("permits an explicit loopback-only development login without WorkOS", async () => {
    const response = await SELF.fetch("http://localhost/api/browser/dev-login", {
      method: "POST",
      headers: { origin: "http://localhost" }
    });
    expect(response.status).toBe(200);
    expect(response.headers.get("set-cookie")).toContain("comet_dev_session=");
    expect(response.headers.get("set-cookie")).not.toContain("Secure");

    const cookie = response.headers.get("set-cookie")!.split(";")[0]!;
    const session = await SELF.fetch("http://localhost/api/browser/session", { headers: { cookie } });
    const body = await session.json() as { authenticated?: boolean; csrfToken?: string; ownerId?: string; organizationId?: string };
    expect(body).toMatchObject({ authenticated: true, ownerId: "browser-e2e-owner", organizationId: "dev-org" });
    const activity = await SELF.fetch("http://localhost/api/browser/activity", { method: "POST", headers: { cookie, origin: "http://localhost", "x-csrf-token": body.csrfToken! } });
    expect(await activity.json()).toEqual({ authenticated: true });
    expect((await SELF.fetch("http://localhost/api/browser/activity", { method: "POST", headers: { cookie, origin: "http://attacker.invalid", "x-csrf-token": body.csrfToken! } })).status).toBe(401);
    expect((await SELF.fetch("https://test/api/browser/dev-login", { method: "POST", headers: { origin: "http://localhost" } })).status).toBe(403);
  });
});
