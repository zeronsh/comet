import { env, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";

const call = async (path: string, body: unknown) => {
  const stub = env.BROWSER_SESSIONS.get(env.BROWSER_SESSIONS.idFromName("browser-sessions-v1"));
  return stub.fetch(new Request(`https://store${path}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) }));
};

describe("BrowserSessionStore", () => {
  it("persists encrypted session state, enforces owner-scoped revoke, and rejects it afterward", async () => {
    const hash = "a".repeat(64);
    const created = await call("/create", { hash, ownerId: "owner-1", providerSessionId: "session-1", csrfToken: "csrf-1", accessToken: "access", refreshToken: "refresh", providerExpiresAt: Date.now() + 60 * 60_000 });
    expect(created.status).toBe(200);

    const valid = await call("/validate", { hash, touch: false });
    expect(await valid.json()).toMatchObject({ authenticated: true, ownerId: "owner-1", providerSessionId: "session-1" });

    await call("/revoke", { hash, ownerId: "other-owner" });
    expect(await (await call("/validate", { hash, touch: false })).json()).toMatchObject({ authenticated: true });

    await call("/revoke", { hash, ownerId: "owner-1" });
    expect(await (await call("/validate", { hash, touch: false })).json()).toEqual({ authenticated: false });
  });


  it("atomically rejects binding a revoked session", async () => {
    const hash = "b".repeat(64);
    await call("/create", { hash, ownerId: "owner-bind", providerSessionId: "provider-bind", csrfToken: "csrf-bind", accessToken: "access", refreshToken: "refresh", providerExpiresAt: Date.now() + 60_000 });
    await call("/revoke", { hash, ownerId: "owner-bind" });
    const bound = await call("/bind", { hash, room: "d2/device", connId: "connection" });
    expect(bound.status).toBe(401);
    expect(await bound.json()).toEqual({ bound: false });
  });


  it("consumes a callback transaction exactly once", async () => {
    const state = `state-${crypto.randomUUID()}`;
    const nonceHash = "b".repeat(64);
    const verifier = "v".repeat(43);
    expect((await call("/transaction", { state, nonceHash, verifier })).status).toBe(200);
    expect(await (await call("/consume-transaction", { state, nonceHash })).json()).toEqual({ verifier });
    expect((await call("/consume-transaction", { state, nonceHash })).status).toBe(401);
  });


  it("permits local validation for logout when an expired provider token cannot refresh", async () => {
    const hash = "e".repeat(64);
    await call("/create", { hash, ownerId: "owner-local", providerSessionId: "provider-local", csrfToken: "csrf-local", accessToken: "access", refreshToken: "refresh", providerExpiresAt: Date.now() - 1 });
    expect(await (await call("/validate", { hash, touch: false, refresh: false })).json()).toMatchObject({ authenticated: true, ownerId: "owner-local" });
    expect(await (await call("/validate", { hash, touch: false, refresh: true })).json()).toEqual({ authenticated: false });
  });


  it("scopes preview capabilities to a live parent browser session", async () => {
    const parent = "p".repeat(64);
    const preview = "q".repeat(64);
    await call("/create", { hash: parent, ownerId: "owner-preview", providerSessionId: "provider-preview", csrfToken: "csrf", accessToken: "access", refreshToken: "refresh", providerExpiresAt: Date.now() + 60_000 });
    const host = "p-0123456789abcdef0123456789abcdef01234567.preview.test";
    expect((await call("/preview-create", { hash: preview, parentHash: parent, ownerId: "owner-preview", deviceId: "device-1", serviceId: "service-1", host })).status).toBe(200);
    expect(await (await call("/preview-validate", { hash: preview })).json()).toMatchObject({ authenticated: true, parentHash: parent, ownerId: "owner-preview", deviceId: "device-1", serviceId: "service-1", host });
    await call("/revoke", { hash: parent, ownerId: "owner-preview" });
    expect(await (await call("/preview-validate", { hash: preview })).json()).toEqual({ authenticated: false });
  });


  it("cancels the alarm after the final session is revoked", async () => {
    const hash = "f".repeat(64);
    const stub = env.BROWSER_SESSIONS.get(env.BROWSER_SESSIONS.idFromName("browser-sessions-v1"));
    await call("/create", { hash, ownerId: "owner-alarm", providerSessionId: "provider-alarm", csrfToken: "csrf-alarm", accessToken: "access", refreshToken: "refresh", providerExpiresAt: Date.now() + 60_000 });
    await call("/revoke", { hash, ownerId: "owner-alarm" });
    expect(await runInDurableObject(stub, async (_instance, state) => state.storage.getAlarm())).toBeNull();
  });
});
