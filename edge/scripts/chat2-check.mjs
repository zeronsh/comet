// chat2 wire-level E2E against a deployed worker (AUTH_MODE=dev).
// Speaks the binary frame protocol from edge/src/chat-frames.ts and drives
// every route + guard in edge/src/chat-room.ts per docs/chat2-sync.md B.
// Usage: node chat2-e2e.mjs <baseUrl>
import { randomUUID, randomBytes } from "node:crypto";

const base = process.argv[2];
if (!base) throw new Error("usage: node chat2-e2e.mjs <baseUrl>");
const wsBase = base.replace(/^http/, "ws");
const userA = "e2e-user-a";
const userB = "e2e-user-b";
const chat = `e2e-${randomUUID().slice(0, 13)}`;

// ── frame codec (mirror of chat-frames.ts) ──────────────────────────────────
const FRAME = { hello: 0x01, state: 0x02, rowsReq: 0x03, row: 0x04, rowsDone: 0x05, push: 0x06, ack: 0x07, presence: 0x08, probe: 0x09, probeOk: 0x0a, error: 0x0b };
const NAME = Object.fromEntries(Object.entries(FRAME).map(([k, v]) => [v, k]));
const enc = (type, header, payload = new Uint8Array(0)) => {
  const h = new TextEncoder().encode(JSON.stringify(header));
  const out = new Uint8Array(5 + h.length + payload.length);
  out[0] = type;
  new DataView(out.buffer).setUint32(1, h.length, true);
  out.set(h, 5);
  out.set(payload, 5 + h.length);
  return out;
};
const dec = (bytes) => {
  const b = new Uint8Array(bytes);
  const len = new DataView(b.buffer, b.byteOffset).getUint32(1, true);
  return { type: b[0], header: JSON.parse(new TextDecoder().decode(b.subarray(5, 5 + len))), payload: b.subarray(5 + len) };
};

// ── tiny WS client with a frame inbox ───────────────────────────────────────
class Client {
  constructor(device, user) { this.device = device; this.user = user; this.inbox = []; this.waiters = []; this.closed = null; }
  async connect() {
    this.ws = new WebSocket(`${wsBase}/chat2/${chat}/ws?device=${this.device}&token=${this.user}`);
    this.ws.binaryType = "arraybuffer";
    this.ws.onmessage = (ev) => { const f = dec(ev.data); this.inbox.push(f); this.waiters.forEach((w) => w()); };
    this.ws.onclose = (ev) => { this.closed = { code: ev.code, reason: ev.reason }; this.waiters.forEach((w) => w()); };
    await new Promise((res, rej) => { this.ws.onopen = res; this.ws.onerror = () => rej(new Error("ws connect failed")); });
  }
  send(type, header, payload) { this.ws.send(enc(type, header, payload)); }
  sendRaw(bytes) { this.ws.send(bytes); }
  async next(type, timeoutMs = 8000, pred = () => true) {
    const start = Date.now();
    for (;;) {
      const i = this.inbox.findIndex((f) => f.type === type && pred(f));
      if (i >= 0) return this.inbox.splice(i, 1)[0];
      if (this.closed) throw new Error(`socket closed (${this.closed.code}) while waiting for ${NAME[type]}`);
      if (Date.now() - start > timeoutMs) throw new Error(`timeout waiting for ${NAME[type]}; inbox=[${this.inbox.map((f) => NAME[f.type])}]`);
      await new Promise((res) => { this.waiters.push(res); setTimeout(res, 150); });
      this.waiters = [];
    }
  }
  async nextAny(types, timeoutMs = 8000) {
    const start = Date.now();
    for (;;) {
      const i = this.inbox.findIndex((f) => types.includes(f.type));
      if (i >= 0) return this.inbox.splice(i, 1)[0];
      if (this.closed) throw new Error(`socket closed (${this.closed.code}) while waiting for ${types.map((t) => NAME[t])}`);
      if (Date.now() - start > timeoutMs) throw new Error(`timeout waiting for ${types.map((t) => NAME[t])}`);
      await new Promise((res) => { this.waiters.push(res); setTimeout(res, 150); });
      this.waiters = [];
    }
  }
  async collectRows(timeoutMs = 8000) {
    const rows = [];
    for (;;) {
      const f = await this.nextAny([FRAME.row, FRAME.rowsDone], timeoutMs);
      if (f.type === FRAME.rowsDone) return { rows, done: f };
      rows.push(f);
    }
  }
  async waitClose(timeoutMs = 8000) {
    const start = Date.now();
    while (!this.closed) {
      if (Date.now() - start > timeoutMs) throw new Error("timeout waiting for close");
      await new Promise((res) => setTimeout(res, 100));
    }
    return this.closed;
  }
  async hello(cursor = 0) { this.send(FRAME.hello, { cursor, device: this.device }); return this.next(FRAME.state); }
}

const http = (path, { method = "GET", user = userA, headers = {}, body } = {}) =>
  fetch(`${base}${path}`, { method, headers: { authorization: `Bearer ${user}`, ...headers }, body });

// ── assertions ──────────────────────────────────────────────────────────────
let pass = 0, fail = 0;
const results = [];
const check = (name, cond, detail = "") => {
  if (cond) { pass++; results.push(`  ok  ${name}`); }
  else { fail++; results.push(`FAIL  ${name}${detail ? ` — ${detail}` : ""}`); }
};
const eqBytes = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

// ════════════════════════════════════════════════════════════════════════════
// 1. Worker gates
{
  const h = await (await fetch(`${base}/health`)).json();
  check("health: dev auth mode", h.ok === true && h.auth === "dev");
  const unauth = await fetch(`${base}/chat2/${chat}/stats`);
  check("unauthenticated → 401", unauth.status === 401, `got ${unauth.status}`);
  const preClaim = await http(`/chat2/${chat}/stats`);
  check("stats before claim → 404", preClaim.status === 404, `got ${preClaim.status}`);
  const badSub = await http(`/chat2/${chat}/bogus`);
  check("unknown subroute → 404", badSub.status === 404, `got ${badSub.status}`);
  const badMethod = await http(`/chat2/${chat}/stats`, { method: "POST" });
  check("wrong method → 404", badMethod.status === 404, `got ${badMethod.status}`);
}

// 2. Basic protocol on device A
const devA = new Client("devA", userA);
await devA.connect();
{
  const state = await devA.hello();
  check("hello → state on fresh room", state.header.headSeq === 0 && state.header.seqFloor === 0 && state.header.checkpointSeq === 0 && state.header.rowCount === 0, JSON.stringify(state.header));
  check("fresh room frontier empty", state.payload.length === 0, `${state.payload.length}B`);
}
const rows = [randomBytes(1024), randomBytes(32 * 1024), randomBytes(200 * 1024)].map((b) => new Uint8Array(b));
for (let i = 0; i < rows.length; i++) {
  devA.send(FRAME.push, { batchId: `batch-${i + 1}` }, rows[i]);
  const ack = await devA.next(FRAME.ack);
  check(`push ${i + 1} acked seq=${i + 1}`, ack.header.seq === i + 1 && ack.header.dup === false, JSON.stringify(ack.header));
}
{
  devA.send(FRAME.push, { batchId: "batch-2" }, new Uint8Array(randomBytes(64)));
  const ack = await devA.next(FRAME.ack);
  check("dup batchId → dup:true, original seq", ack.header.dup === true && ack.header.seq === 2, JSON.stringify(ack.header));
  devA.send(FRAME.probe, {});
  const p = await devA.next(FRAME.probeOk);
  check("probe → probeOk{headSeq:3} (dup not appended)", p.header.headSeq === 3, JSON.stringify(p.header));
  devA.send(FRAME.rowsReq, { after: 0 });
  const { rows: seen, done } = await devA.collectRows();
  check("rowsReq → rowsDone{headSeq:3}", done.header.headSeq === 3, JSON.stringify(done.header));
  check("backfill: 3 rows, ordered, bytes intact", seen.length === 3 && seen.every((f, i) => f.header.seq === i + 1 && f.header.device === "devA" && eqBytes(f.payload, rows[i])), `got ${seen.length} rows`);
}

// 3. Pre-hello + malformed-frame handling on a fresh socket
{
  const pre = new Client("devPre", userA);
  await pre.connect();
  pre.send(FRAME.rowsReq, { after: 0 });
  const err = await pre.next(FRAME.error);
  check("rowsReq before hello → hello_first error", err.header.code === "hello_first", JSON.stringify(err.header));
  pre.sendRaw(new Uint8Array([0x7f, 1, 0, 0, 0, 123]));
  const err2 = await pre.next(FRAME.error);
  check("unknown frame type → bad_frame error (socket open)", err2.header.code === "bad_frame");
  pre.send(FRAME.probe, {});
  await pre.next(FRAME.probeOk);
  check("socket still usable after bad frame", true);
  pre.ws.send("hello as text");
  const closed = await pre.waitClose();
  check("text message → close 1003", closed.code === 1003, `got ${closed.code}`);
}

// 4. Second device: backfill, live relay, excludeOwn, presence
const devB = new Client("devB", userA);
await devB.connect();
{
  const state = await devB.hello();
  check("devB hello sees headSeq=3", state.header.headSeq === 3, JSON.stringify(state.header));
  const live = new Uint8Array(randomBytes(5 * 1024));
  devA.send(FRAME.push, { batchId: "batch-live-4" }, live);
  const [ack, relayed] = await Promise.all([devA.next(FRAME.ack), devB.next(FRAME.row)]);
  check("live relay: devB got devA's push", relayed.header.seq === 4 && relayed.header.device === "devA" && eqBytes(relayed.payload, live), JSON.stringify(relayed.header));
  check("sender gets ack only", ack.header.seq === 4 && devA.inbox.every((f) => f.type !== FRAME.row), `inbox=[${devA.inbox.map((f) => NAME[f.type])}]`);
  const fromB = new Uint8Array(randomBytes(2048));
  devB.send(FRAME.push, { batchId: "batch-b-5" }, fromB);
  const [, toA] = await Promise.all([devB.next(FRAME.ack), devA.next(FRAME.row)]);
  check("relay works both directions", toA.header.seq === 5 && toA.header.device === "devB");

  const devA2 = new Client("devA", userA); // reconnect path
  await devA2.connect();
  await devA2.hello(4);
  devA2.send(FRAME.rowsReq, { after: 0, excludeOwn: true });
  const { rows: got, done: exDone } = await devA2.collectRows();
  check("excludeOwn rowsDone still reports true headSeq=5", exDone.header.headSeq === 5, JSON.stringify(exDone.header));
  check("excludeOwn: only devB's row returned", got.length === 1 && got[0].header.device === "devB" && got[0].header.seq === 5, `got ${got.length}`);
  devA2.ws.close();

  const beat = new Uint8Array(randomBytes(48));
  devA.send(FRAME.presence, { at: Date.now() }, beat);
  const seen = await devB.next(FRAME.presence);
  check("presence relayed with payload", seen.header.device === "devA" && eqBytes(seen.payload, beat));
}

// 5. Ownership
{
  const asB = await http(`/chat2/${chat}/stats`, { user: userB });
  check("other user stats → 403", asB.status === 403, `got ${asB.status}`);
  let wsRejected = false;
  try { const c = new Client("devX", userB); await c.connect(); await c.waitClose(3000); wsRejected = true; } catch { wsRejected = true; }
  check("other user WS → rejected", wsRejected);
  const cpB = await http(`/chat2/${chat}/checkpoint?seqCovered=1`, { method: "POST", user: userB, headers: { "x-chat2-frontier": Buffer.from([1, 2, 3]).toString("base64") }, body: new Uint8Array([1]) });
  check("other user checkpoint POST → 403", cpB.status === 403, `got ${cpB.status}`);
}

// 6. Checkpoint lifecycle
const frontier = new Uint8Array(randomBytes(32));
const ckpt = new Uint8Array(randomBytes(300 * 1024));
{
  const res = await http(`/chat2/${chat}/checkpoint?seqCovered=5`, { method: "POST", headers: { "x-chat2-frontier": Buffer.from(frontier).toString("base64") }, body: ckpt });
  const body = await res.json();
  check("checkpoint commit → pruned 5 rows", res.status === 200 && body.seqFloor === 5 && body.pruned === 5, JSON.stringify(body));
  const stats = await (await http(`/chat2/${chat}/stats`)).json();
  check("stats after checkpoint", stats.headSeq === 5 && stats.seqFloor === 5 && stats.rowCount === 0 && stats.checkpointSeq === 5 && stats.checkpointSize === ckpt.length, JSON.stringify(stats));
  const get = await http(`/chat2/${chat}/checkpoint`);
  const bytes = new Uint8Array(await get.arrayBuffer());
  check("GET checkpoint round-trips", get.status === 200 && get.headers.get("x-chat2-checkpoint-seq") === "5" && eqBytes(bytes, ckpt));
  const part = await http(`/chat2/${chat}/checkpoint`, { headers: { range: "bytes=100000-" } });
  const partBytes = new Uint8Array(await part.arrayBuffer());
  check("Range resume → 206 correct slice", part.status === 206 && part.headers.get("content-range") === `bytes 100000-${ckpt.length - 1}/${ckpt.length}` && eqBytes(partBytes, ckpt.subarray(100000)), `${part.status} ${part.headers.get("content-range")}`);
  const past = await http(`/chat2/${chat}/checkpoint`, { headers: { range: `bytes=${ckpt.length}-` } });
  check("Range past end → 416", past.status === 416, `got ${past.status}`);
  const reg = await http(`/chat2/${chat}/checkpoint?seqCovered=4`, { method: "POST", headers: { "x-chat2-frontier": Buffer.from([1, 2, 3]).toString("base64") }, body: new Uint8Array([1]) });
  check("floor regression → 409", reg.status === 409 && (await reg.json()).error === "floor_regression");
  const ahead = await http(`/chat2/${chat}/checkpoint?seqCovered=99`, { method: "POST", headers: { "x-chat2-frontier": Buffer.from([1, 2, 3]).toString("base64") }, body: new Uint8Array([1]) });
  check("ahead of head → 409", ahead.status === 409 && (await ahead.json()).error === "ahead_of_head");
  const empty = await http(`/chat2/${chat}/checkpoint?seqCovered=5`, { method: "POST", headers: { "x-chat2-frontier": Buffer.from([1, 2, 3]).toString("base64") }, body: new Uint8Array(0) });
  check("empty checkpoint → 409", empty.status === 409);
  const emptyFrontier = await http(`/chat2/${chat}/checkpoint?seqCovered=5`, { method: "POST", headers: { "x-chat2-frontier": "" }, body: new Uint8Array([1]) });
  check(emptyFrontier.status === 400, "empty frontier on a content checkpoint rejected (parked-rows poison, 2026-08-18)");
  const badFrontier = await http(`/chat2/${chat}/checkpoint?seqCovered=5`, { method: "POST", headers: { "x-chat2-frontier": "!!!not-base64!!!" }, body: new Uint8Array([1]) });
  check("malformed frontier → 400", badFrontier.status === 400, `got ${badFrontier.status}`);
  const fresh = new Client("devC", userA);
  await fresh.connect();
  const st = await fresh.hello();
  check("hello carries checkpoint frontier bytes", eqBytes(st.payload, frontier) && st.header.checkpointSeq === 5);
  fresh.ws.close();
  devA.send(FRAME.push, { batchId: "batch-post-ckpt-6" }, new Uint8Array(randomBytes(512)));
  const ack = await devA.next(FRAME.ack);
  check("seq continues past checkpoint floor", ack.header.seq === 6, JSON.stringify(ack.header));
}

// 7. Sidecars
{
  const tail = JSON.stringify({ entries: [{ id: 1, text: "hello tail" }] });
  const put = await http(`/chat2/${chat}/tail`, { method: "PUT", headers: { "content-type": "application/json" }, body: tail });
  check("PUT tail sidecar", put.status === 200);
  const get = await http(`/chat2/${chat}/tail`);
  check("GET tail verbatim + content-type", (await get.text()) === tail && get.headers.get("content-type") === "application/json");
  const diffBytes = new Uint8Array(randomBytes(10 * 1024));
  await http(`/chat2/${chat}/diff`, { method: "PUT", headers: { "content-type": "application/octet-stream" }, body: diffBytes });
  const gd = await http(`/chat2/${chat}/diff`);
  check("diff sidecar round-trips", eqBytes(new Uint8Array(await gd.arrayBuffer()), diffBytes));
  const big = await http(`/chat2/${chat}/tail`, { method: "PUT", body: new Uint8Array(4 * 1024 * 1024 + 1) });
  check("oversized sidecar → 413", big.status === 413, `got ${big.status}`);
}

// 8. Caps + quota
{
  devA.send(FRAME.push, { batchId: "batch-too-big" }, new Uint8Array(1024 * 1024 + 1));
  const err = await devA.next(FRAME.error);
  check("row > 1MB → too_large error, socket open", err.header.code === "too_large");
  check("too_large error carries batchId (F2 retirement)", err.header.batchId === "batch-too-big", JSON.stringify(err.header));
  devA.send(FRAME.probe, {});
  await devA.next(FRAME.probeOk);
  check("socket survives too_large", true);

  const huge = new Client("devHuge", userA);
  await huge.connect();
  await huge.hello();
  huge.send(FRAME.push, { batchId: "b" }, new Uint8Array(1024 * 1024 + 16 * 1024));
  const closed = await huge.waitClose();
  check("frame > cap → close 1009", closed.code === 1009, `got ${closed.code}`);

  const q = new Client("devQ", userA);
  await q.connect();
  await q.hello();
  let quotaErr = null, acks = 0;
  for (let i = 0; i < 305 && !quotaErr; i++) {
    q.send(FRAME.push, { batchId: `q-${i}` }, new Uint8Array([1, 2, 3]));
    const f = await Promise.race([q.next(FRAME.ack, 8000).catch(() => null), q.next(FRAME.error, 8000).catch(() => null)]);
    if (f?.type === FRAME.error) quotaErr = { at: i, code: f.header.code, batchId: f.header.batchId };
    else if (f?.type === FRAME.ack) acks++;
  }
  check("push quota trips at 300/min", quotaErr?.code === "quota" && acks === 300, JSON.stringify({ quotaErr, acks }));
  check("quota error carries batchId (transient retry)", quotaErr?.batchId === `q-${quotaErr?.at}`, JSON.stringify(quotaErr));
  q.ws.close();
  const stats = await (await http(`/chat2/${chat}/stats`)).json();
  check("stats: pushOutcomes attribution", stats.pushOutcomes?.devQ?.ok === 300 && stats.pushOutcomes?.devQ?.rejected >= 1 && stats.pushOutcomes?.devA?.ok >= 5, JSON.stringify(stats.pushOutcomes ?? {}));
  check("stats: presence + sockets present", typeof stats.connectedSockets === "number" && typeof stats.presence === "object");
}

// 9. Blob sidecar routes (workstream A)
{
  const text = "full tool output — line one\nline two, much longer than the 160-char summary would allow…";
  const put = await http(`/blob/${chat}/part-01`, { method: "PUT", headers: { "content-type": "text/plain; charset=utf-8" }, body: text });
  check("PUT blob", put.status === 200, `got ${put.status}`);
  const get = await http(`/blob/${chat}/part-01`);
  check("GET blob round-trips + content-type", (await get.text()) === text && (get.headers.get("content-type") ?? "").startsWith("text/plain"));
  const crossUser = await http(`/blob/${chat}/part-01`, { user: userB });
  check("cross-user blob GET → 404 (key isolation)", crossUser.status === 404, `got ${crossUser.status}`);
  const traversal = await http(`/blob/${chat}/${encodeURIComponent("../escape")}`, { method: "PUT", body: "x" });
  check("path-traversal partId rejected", traversal.status === 404 || traversal.status === 400, `got ${traversal.status}`);
  const tooBig = await http(`/blob/${chat}/part-big`, { method: "PUT", body: new Uint8Array(1024 * 1024 + 1) });
  check("blob > 1MB → 413", tooBig.status === 413, `got ${tooBig.status}`);
  const diffKey = await http(`/blob/${chat}/part-01.diff`, { method: "PUT", body: "diff body" });
  check("'.diff'-suffixed part key accepted", diffKey.status === 200);
  // '#'-bearing part ids travel percent-encoded (the doc_host fix): two ids
  // that used to silently collide on the truncated key must stay distinct.
  const hash1 = await http(`/blob/${chat}/${encodeURIComponent("m1#c1")}`, { method: "PUT", body: "payload of m1#c1" });
  const hash2 = await http(`/blob/${chat}/${encodeURIComponent("m1#c2")}`, { method: "PUT", body: "payload of m1#c2" });
  check("encoded '#' part ids accepted", hash1.status === 200 && hash2.status === 200, `${hash1.status}/${hash2.status}`);
  const hashGet1 = await (await http(`/blob/${chat}/${encodeURIComponent("m1#c1")}`)).text();
  const hashGet2 = await (await http(`/blob/${chat}/${encodeURIComponent("m1#c2")}`)).text();
  check("'#' part ids resolve distinct keys (no collision)", hashGet1 === "payload of m1#c1" && hashGet2 === "payload of m1#c2", `${hashGet1} / ${hashGet2}`);
  const truncated = await http(`/blob/${chat}/m1`);
  check("truncated key 'm1' does NOT exist (old bug shape)", truncated.status === 404, `got ${truncated.status}`);
  const badEscape = await http(`/blob/${chat}/bad%zzescape`, { method: "PUT", body: "x" });
  check("malformed %-escape → 400", badEscape.status === 400, `got ${badEscape.status}`);
}

// 10. Reset (operator wipe)
{
  const res = await http(`/chat2/${chat}/reset`, { method: "POST" });
  check("reset → ok", res.status === 200);
  const closedA = await devA.waitClose(8000);
  const closedB = await devB.waitClose(8000);
  check("reset closes sockets 4410", closedA.code === 4410 && closedB.code === 4410, `${closedA.code}/${closedB.code}`);
  const stats = await http(`/chat2/${chat}/stats`);
  check("room unclaimed after reset (stats 404)", stats.status === 404, `got ${stats.status}`);
  const re = new Client("devA", userA);
  await re.connect();
  const st = await re.hello();
  check("re-claim after reset: fresh log", st.header.headSeq === 0 && st.payload.length === 0, JSON.stringify(st.header));
  re.ws.close();
}

console.log(`\nchat2 E2E vs ${base}\nroom: chat2/${chat}\n`);
console.log(results.join("\n"));
console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail > 0 ? 1 : 0);
