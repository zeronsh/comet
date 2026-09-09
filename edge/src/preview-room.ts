import { AUTH_USER_HEADER, type Env } from "./env";

const ID = /^[A-Za-z0-9_-]{1,128}$/;
const HOST = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.localhost$/;
const MAX_MESSAGE = 1024 * 1024;
type Attachment = { device: string; connection: string; connected: number; lastSeen: number; budgetAt: number; messages: number };
type Service = Record<string, string | number | boolean>;

export function previewCatalog(value: unknown, device: string): Service[] | undefined {
  if (!Array.isArray(value) || value.length > 256) return;
  const ids = new Set<string>();
  const output: Service[] = [];
  for (const item of value) {
    if (!item || typeof item !== "object" || Array.isArray(item)) return;
    const s = item as Record<string, unknown>;
    const strings = ["id", "projectId", "projectName", "projectCwd", "deviceId", "deviceName", "hostname", "name", "cwd"];
    if (!strings.every(k => typeof s[k] === "string" && (s[k] as string).length <= (k === "cwd" || k === "projectCwd" ? 4096 : 128))) return;
    if (s.deviceId !== device || !ID.test(s.id as string) || ids.has(s.id as string) || !HOST.test(s.hostname as string)) return;
    if (!Number.isInteger(s.port) || (s.port as number) < 1 || (s.port as number) > 65535 || !Number.isSafeInteger(s.pid) || (s.pid as number) < 1 || !Number.isSafeInteger(s.startedAt) || (s.startedAt as number) < 0 || typeof s.zeronOwned !== "boolean") return;
    ids.add(s.id as string);
    const clean: Service = {};
    for (const key of [...strings, "port", "pid", "startedAt", "zeronOwned"]) clean[key] = s[key] as string | number | boolean;
    output.push(clean);
  }
  return output;
}
export function previewSignal(value: unknown): object | undefined {
  if (!value || typeof value !== "object") return;
  const s = value as Record<string, unknown>;
  if (typeof s.session !== "string" || !ID.test(s.session)) return;
  if (s.kind === "connect" && s.sdp === undefined) return { kind: "connect", session: s.session };
  if ((s.kind !== "offer" && s.kind !== "answer") || !s.sdp || typeof s.sdp !== "object") return;
  const desc = s.sdp as Record<string, unknown>;
  if (desc.type !== s.kind || typeof desc.sdp !== "string" || desc.sdp.length > 65536 || !desc.sdp.startsWith("v=0\r\n") || !desc.sdp.includes("a=fingerprint:sha-256 ")) return;
  return { kind: s.kind, session: s.session, sdp: { type: desc.type, sdp: desc.sdp } };
}

/** One authenticated user's devices in one organization. Only presence, service
 * metadata and SDP/ICE coordination live here; binary preview frames are rejected. */
export class PreviewRoom {
  constructor(private state: DurableObjectState, _env: Env) {
    state.storage.sql.exec("CREATE TABLE IF NOT EXISTS preview_catalog (device TEXT PRIMARY KEY, services TEXT NOT NULL)");
    state.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }
  async fetch(request: Request): Promise<Response> {
    if (!request.headers.get(AUTH_USER_HEADER)) return new Response("Unauthorized", { status: 401 });
    if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") return new Response("Expected WebSocket", { status: 426 });
    const device = new URL(request.url).searchParams.get("device") ?? "";
    if (!ID.test(device)) return new Response("Invalid device", { status: 400 });
    const existing = this.sockets().find(ws => this.info(ws).device === device);
    if (!existing && this.sockets().length >= 16) return new Response("Device limit reached", { status: 429 });
    if (existing) { existing.serializeAttachment({ ...this.info(existing), device: "" }); existing.close(1000, "Device reconnected"); }
    this.state.storage.sql.exec("DELETE FROM preview_catalog WHERE device = ?", device);
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    this.state.acceptWebSocket(server);
    const now = Date.now();
    server.serializeAttachment({ device, connection: crypto.randomUUID(), connected: now, lastSeen: now, budgetAt: now, messages: 0 } satisfies Attachment);
    this.broadcast({ type: "catalog", device, services: [] }, server);
    for (const other of this.sockets()) {
      const peer = this.info(other).device;
      if (peer && peer !== device) this.send(server, { type: "catalog", device: peer, services: this.catalog(peer) });
    }
    await this.state.storage.setAlarm(now + 30000);
    return new Response(null, { status: 101, webSocket: client });
  }
  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const info = this.info(ws);
    if (!info.device) return;
    if (typeof message !== "string" || new TextEncoder().encode(message).byteLength > MAX_MESSAGE) { ws.close(1008, "Signaling messages only"); return; }
    const now = Date.now();
    if (now - info.budgetAt > 10000) { info.budgetAt = now; info.messages = 0; }
    if (++info.messages > 120) { ws.close(1008, "Signaling rate exceeded"); return; }
    info.lastSeen = now; ws.serializeAttachment(info);
    let value: Record<string, unknown>;
    try { value = JSON.parse(message); } catch { ws.close(1008, "Invalid signaling JSON"); return; }
    if (!value || typeof value !== "object") { ws.close(1008, "Invalid signaling message"); return; }
    if (value.type === "catalog") {
      const services = previewCatalog(value.services, info.device);
      if (!services) { ws.close(1008, "Invalid preview catalog"); return; }
      this.state.storage.sql.exec("INSERT OR REPLACE INTO preview_catalog(device, services) VALUES (?, ?)", info.device, JSON.stringify(services));
      this.broadcast({ type: "catalog", device: info.device, services }, ws);
    } else if (value.type === "signal" && typeof value.to === "string" && ID.test(value.to) && value.to !== info.device) {
      const signal = previewSignal(value.signal);
      if (!signal) { ws.close(1008, "Invalid preview signaling"); return; }
      const target = this.sockets().find(peer => this.info(peer).device === value.to);
      if (target) this.send(target, { type: "signal", from: info.device, signal });
      else this.send(ws, { type: "gone", device: value.to });
    } else { ws.close(1008, "Unsupported preview message"); }
  }
  async webSocketClose(ws: WebSocket): Promise<void> { await this.remove(ws); }
  async webSocketError(ws: WebSocket): Promise<void> { await this.remove(ws); }
  async alarm(): Promise<void> {
    const now = Date.now();
    for (const ws of this.sockets()) {
      const info = this.info(ws);
      const pong = this.state.getWebSocketAutoResponseTimestamp(ws)?.getTime() ?? 0;
      // Periodic reconnect obtains a fresh access token. A half-open device
      // disappears promptly, including after this object hibernates.
      if (now - Math.max(info.lastSeen, pong) > 45000 || now - info.connected > 15 * 60000) {
        await this.remove(ws); ws.close(1000, "Refresh device presence");
      }
    }
    if (this.sockets().some(ws => this.info(ws).device)) await this.state.storage.setAlarm(now + 30000);
  }
  private catalog(device: string): Service[] {
    const row = this.state.storage.sql.exec<{ services: string }>("SELECT services FROM preview_catalog WHERE device = ?", device).toArray()[0];
    return row ? JSON.parse(row.services) as Service[] : [];
  }
  private sockets(): WebSocket[] { return this.state.getWebSockets().filter(ws => this.info(ws)?.device); }
  private info(ws: WebSocket): Attachment { return ws.deserializeAttachment() as Attachment; }
  private send(ws: WebSocket, value: object): void { try { ws.send(JSON.stringify(value)); } catch { ws.close(1011, "Signaling disconnected"); } }
  private broadcast(value: object, except: WebSocket): void { for (const ws of this.sockets()) if (ws !== except) this.send(ws, value); }
  private async remove(ws: WebSocket): Promise<void> {
    const info = this.info(ws);
    if (!info?.device) return;
    const device = info.device; ws.serializeAttachment({ ...info, device: "" });
    this.state.storage.sql.exec("DELETE FROM preview_catalog WHERE device = ?", device);
    this.broadcast({ type: "gone", device }, ws);
  }
}
