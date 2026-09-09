import { SELF, env, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
function next(ws: WebSocket): Promise<Record<string, any>> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error("signaling timed out")), 2000);
    ws.addEventListener("message", event => { clearTimeout(timeout); resolve(JSON.parse(event.data as string)); }, { once: true });
  });
}
async function connect(user: string, device: string, org = "org") {
  const response = await SELF.fetch(`https://test/preview/${org}/ws?device=${device}`, { headers: { authorization: `Bearer ${user}@${org}`, upgrade: "websocket", "x-zeron-auth-user": "spoofed" } });
  expect(response.status).toBe(101); const ws = response.webSocket!; ws.accept(); return ws;
}
describe("authenticated preview coordination on workerd", () => {
  it("checks organization membership before routing", async () => {
    expect((await SELF.fetch("https://test/preview/foreign/ws?device=a", { headers: { authorization: "Bearer user@org", upgrade: "websocket" } })).status).toBe(403);
    expect((await SELF.fetch("https://test/preview/org/ws?device=a", { headers: { upgrade: "websocket" } })).status).toBe(401);
  });
  it("stamps sender identity, isolates users, removes departed devices and rejects bytes", async () => {
    const user = crypto.randomUUID(); const a = await connect(user, "a");
    const aCatalog = next(a); const b = await connect(user, "b"); const bCatalog = next(b);
    expect((await aCatalog).device).toBe("b"); expect((await bCatalog).device).toBe("a");
    const other = await connect(`${user}-other`, "b");
    const received = next(b);
    a.send(JSON.stringify({ type: "signal", to: "b", from: "spoofed", signal: { kind: "connect", session: "session" } }));
    expect(await received).toEqual({ type: "signal", from: "a", signal: { kind: "connect", session: "session" } });
    const room = env.PREVIEW_ROOMS.get(env.PREVIEW_ROOMS.idFromName(`preview1/org/${user}`));
    await runInDurableObject(room, (_instance, state) => { expect(state.getWebSockets().length).toBe(2); });
    const gone = next(a); b.send(new Uint8Array([1,2,3]));
    // The close handshake completes before presence is removed.
    await new Promise<void>(resolve => b.addEventListener("close", () => resolve(), { once: true }));
    b.close(); expect((await gone).type).toBe("gone");
    a.close(); other.close();
  });
});
