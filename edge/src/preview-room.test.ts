import { describe, expect, it } from "vitest";
import { previewCatalog, previewSignal } from "./preview-room";
const service = { id: "service", projectId: "project", projectName: "app", projectCwd: "/work/app", deviceId: "device", deviceName: "Laptop", hostname: "laptop.app.localhost", name: "Vite", cwd: "/work/app", port: 5173, pid: 123, startedAt: 1000, zeronOwned: true };
describe("preview coordinator admission", () => {
  it("accepts bounded service metadata and strips extra fields", () => {
    expect(previewCatalog([{ ...service, body: "not application traffic" }], "device")).toEqual([service]);
    for (const change of [{ deviceId: "spoofed" }, { hostname: "example.com" }, { hostname: "x.y.localhost.evil.test" }, { port: 0 }, { pid: -1 }, { cwd: "x".repeat(4097) }]) expect(previewCatalog([{ ...service, ...change }], "device")).toBeUndefined();
    expect(previewCatalog([service, service], "device")).toBeUndefined();
    expect(previewCatalog(Array(257).fill(service), "device")).toBeUndefined();
  });
  it("only relays pairing and authenticated SDP/ICE, never proxy frames", () => {
    const sdp = "v=0\r\na=fingerprint:sha-256 AA:BB\r\na=candidate:1 1 udp 1 127.0.0.1 1234 typ host\r\n";
    expect(previewSignal({ kind: "offer", session: "pair", sdp: { type: "offer", sdp }, body: "ignored" })).toEqual({ kind: "offer", session: "pair", sdp: { type: "offer", sdp } });
    expect(previewSignal({ kind: "connect", session: "pair" })).toEqual({ kind: "connect", session: "pair" });
    for (const value of [{ kind: "DATA", session: "pair" }, { kind: "offer", session: "pair", sdp: { type: "answer", sdp } }, { kind: "offer", session: "pair", sdp: { type: "offer", sdp: "HTTP/1.1 200 OK" } }]) expect(previewSignal(value)).toBeUndefined();
  });
});
