import { SELF, env, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { decodeDeviceFrame, encodeDeviceFrame } from "../../src/device-room";
import { SESSION_HASH_HEADER, SESSION_ROOM_HEADER, tokenHash } from "../../src/browser-sessions";
import { AUTH_USER_HEADER } from "../../src/env";

const store = () => env.BROWSER_SESSIONS.get(env.BROWSER_SESSIONS.idFromName("browser-sessions-v1"));
const device = (name: string) => env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(name));

const post = (path: string, body: unknown) =>
  store().fetch(new Request(`https://store${path}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) }));

const join = (room: DurableObjectStub, role: "host" | "client", user: string, connId: string, headers: HeadersInit = {}) =>
  room.fetch(new Request(`https://device/ws?role=${role}&connId=${connId}`, { headers: { upgrade: "websocket", [AUTH_USER_HEADER]: user, ...headers } }));

const nextMessage = (socket: WebSocket) =>
  new Promise<MessageEvent>((resolve) => socket.addEventListener("message", resolve, { once: true }));

const bytes = async (value: unknown): Promise<Uint8Array> => {
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  if (value instanceof Blob) return new Uint8Array(await value.arrayBuffer());
  throw new Error(`unexpected WebSocket message type: ${typeof value}`);
};

describe("DeviceRoom browser binding", () => {
  it("relays native frames for a bound browser client, rejects another owner, and revocation cleans only the client", async () => {
    const roomName = `d2/device-${crypto.randomUUID()}`;
    const room = device(roomName);
    const hash = "c".repeat(64);
    await post("/create", {
      hash,
      ownerId: "owner-1",
      providerSessionId: "provider-1",
      csrfToken: "csrf",
      accessToken: "access",
      refreshToken: "refresh",
      providerExpiresAt: Date.now() + 60 * 60_000
    });

    const hostResponse = await join(room, "host", "owner-1", "host-1");
    const host = hostResponse.webSocket!;
    host.accept();

    const rejected = await join(room, "client", "other-owner", "other-client");
    expect(rejected.status).toBe(403);

    const browserResponse = await join(room, "client", "owner-1", "browser-client", {
      [SESSION_HASH_HEADER]: hash,
      [SESSION_ROOM_HEADER]: roomName
    });

    expect(browserResponse.status).toBe(101);
    const browser = browserResponse.webSocket!;
    browser.accept();

    const firstAlarm = await runInDurableObject(room, async (_instance, state) => state.storage.getAlarm());
    const hash2 = "d".repeat(64);
    await post("/create", { hash: hash2, ownerId: "owner-1", providerSessionId: "provider-2", csrfToken: "csrf-2", accessToken: "access", refreshToken: "refresh", providerExpiresAt: Date.now() + 60 * 60_000 });
    const secondResponse = await join(room, "client", "owner-1", "browser-client-2", { [SESSION_HASH_HEADER]: hash2, [SESSION_ROOM_HEADER]: roomName });
    secondResponse.webSocket!.accept();
    const secondAlarm = await runInDurableObject(room, async (_instance, state) => state.storage.getAlarm());
    expect(secondAlarm).toBeLessThanOrEqual(firstAlarm!);

    const received = nextMessage(host);
    browser.send(encodeDeviceFrame({ s: "rpc-1", k: "rpc" }, new Uint8Array([7, 8])));
    const frame = decodeDeviceFrame(await bytes((await received).data));
    expect(frame.header).toEqual({ s: "rpc-1", k: "rpc", from: "browser-client" });
    expect([...frame.payload]).toEqual([7, 8]);

    const closed = nextMessage(host);
    await post("/revoke", { hash, ownerId: "owner-1" });
    const cleanup = decodeDeviceFrame(await bytes((await closed).data));
    expect(cleanup.header).toMatchObject({ k: " relay", from: "browser-client" });

    // Browser cleanup must not supersede or close the native host socket.
    expect(host.readyState).toBe(WebSocket.OPEN);
  });

  it("upgrades the production browser cookie route and relays RPC handshake frames", async () => {
    const roomName = `d2/device-${crypto.randomUUID()}`;
    const room = device(roomName);
    const raw = `cookie-${crypto.randomUUID()}`;
    const hash = await tokenHash(raw);
    await post("/create", {
      hash,
      ownerId: "owner-1",
      providerSessionId: "provider-1",
      csrfToken: "csrf",
      accessToken: "access",
      refreshToken: "refresh",
      providerExpiresAt: Date.now() + 60 * 60_000
    });
    const hostResponse = await join(room, "host", "owner-1", "host-1");
    const host = hostResponse.webSocket!;
    host.accept();

    const response = await SELF.fetch(`https://test/api/browser/device/${roomName.slice(3)}/ws`, {
      headers: {
        upgrade: "websocket",
        origin: "https://test",
        cookie: `__Host-comet_session=${raw}`
      }
    });
    expect(response.status).toBe(101);
    const browser = response.webSocket!;
    browser.accept();

    const nativeReceived = nextMessage(host);
    browser.send(encodeDeviceFrame({ s: "engine-info", k: "rpc" }, new TextEncoder().encode('{"id":1,"method":"EngineInfo"}')));
    const request = decodeDeviceFrame(await bytes((await nativeReceived).data));
    expect(request.header).toMatchObject({ s: "engine-info", k: "rpc" });
    expect(request.header.from).toEqual(expect.any(String));

    const browserReceived = nextMessage(browser);
    host.send(encodeDeviceFrame({ s: "engine-info", k: "rpc", to: request.header.from }, new TextEncoder().encode('{"id":1,"result":{"ready":true}}')));
    const reply = decodeDeviceFrame(await bytes((await browserReceived).data));
    expect(reply.header).toEqual({ s: "engine-info", k: "rpc" });
    expect(new TextDecoder().decode(reply.payload)).toContain('"ready":true');


    const readyReceived = nextMessage(host);
    browser.send(encodeDeviceFrame({ s: "engine-ready", k: "rpc" }, new TextEncoder().encode('{"id":2,"method":"EngineReady"}')));
    const ready = decodeDeviceFrame(await bytes((await readyReceived).data));
    expect(ready.header).toMatchObject({ s: "engine-ready", k: "rpc", from: request.header.from });

    const readyReply = nextMessage(browser);
    host.send(encodeDeviceFrame({ s: "engine-ready", k: "rpc", to: ready.header.from }, new TextEncoder().encode('{"id":2,"result":{"ready":true}}')));
    expect(new TextDecoder().decode((decodeDeviceFrame(await bytes((await readyReply).data))).payload)).toContain('"ready":true');
  });

});
