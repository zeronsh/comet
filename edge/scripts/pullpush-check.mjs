// Checks the plain-HTTPS pull/push endpoints (450kbps cold-open PR) against
// a running `wrangler dev --var AUTH_MODE:dev` on :8787.
import { randomUUID } from "node:crypto";

const base = process.env.EDGE_URL ?? "http://127.0.0.1:8787";
const org = "pporg";
const user = "ppuser";
const token = `${user}@${org}`;
const chatId = randomUUID();
const device = "pp-dev-a";
let failures = 0;
const ok = (m) => console.log(`ok: ${m}`);
const fail = (m) => {
  console.error(`FAIL: ${m}`);
  failures += 1;
};

const auth = { authorization: `Bearer ${token}` };

// ── registry: push over HTTP, then pull the delta ─────────────────────────
{
  const now = Date.now();
  const hlc = `${String(now).padStart(13, "0")}-000001-${device}`;
  const op = {
    kind: "chats",
    id: "pp-chat-1",
    op: "upsert",
    hlc,
    set: { id: "pp-chat-1", deviceId: device, title: "pull push check", createdAt: now }
  };
  const res = await fetch(`${base}/registry/${org}/push?device=${device}`, {
    method: "POST",
    headers: { ...auth, "content-type": "application/json" },
    body: JSON.stringify({ batch: "pp-batch-1", ops: [op] })
  });
  const ack = await res.json();
  if (res.status !== 200 || ack.batch !== "pp-batch-1" || !(ack.seq >= 1)) {
    fail(`registry push: ${res.status} ${JSON.stringify(ack)}`);
  } else ok(`registry push acked seq=${ack.seq} applied=${ack.applied}`);

  // replay: LWW no-op, still acked
  const replay = await fetch(`${base}/registry/${org}/push?device=${device}`, {
    method: "POST",
    headers: { ...auth, "content-type": "application/json" },
    body: JSON.stringify({ batch: "pp-batch-1", ops: [op] })
  });
  const rack = await replay.json();
  if (replay.status !== 200 || rack.applied !== 0) {
    fail(`registry push replay should apply 0 ops: ${JSON.stringify(rack)}`);
  } else ok("registry push replay is a no-op");

  const full = await (
    await fetch(`${base}/registry/${org}/rows?device=${device}&beat=1`, { headers: auth })
  ).json();
  if (!full.full || !Array.isArray(full.rows) || full.rows.length < 1) {
    fail(`registry pull full: ${JSON.stringify(full).slice(0, 200)}`);
  } else ok(`registry pull (no cursor) full=${full.full} rows=${full.rows.length}`);
  if (!(full.presence && full.presence[device])) fail("beat=1 did not record presence");
  else ok("pull beat recorded presence");

  const delta = await (
    await fetch(`${base}/registry/${org}/rows?since=${full.seq}`, { headers: auth })
  ).json();
  if (delta.full !== false || delta.rows.length !== 0) {
    fail(`registry pull at head should be an empty delta: ${JSON.stringify(delta).slice(0, 200)}`);
  } else ok("registry pull at cursor is an empty delta");
}

// ── chat2: push rows over HTTP, pull framed backfill ──────────────────────
const FRAME = { state: 0x02, row: 0x04, rowsDone: 0x05 };
const decodeFrames = (buf) => {
  const bytes = new Uint8Array(buf);
  const frames = [];
  let off = 0;
  while (off + 4 <= bytes.length) {
    const len = new DataView(bytes.buffer, bytes.byteOffset + off).getUint32(0, true);
    off += 4;
    const frame = bytes.subarray(off, off + len);
    const type = frame[0];
    const headerLen = new DataView(frame.buffer, frame.byteOffset + 1).getUint32(0, true);
    const header = JSON.parse(new TextDecoder().decode(frame.subarray(5, 5 + headerLen)));
    const payload = frame.subarray(5 + headerLen);
    frames.push({ type, header, payload });
    off += len;
  }
  return frames;
};

{
  // claim the room via a checkpoint POST (owner gate), then push rows
  const seed = await fetch(`${base}/chat2/${chatId}/checkpoint?seqCovered=0`, {
    method: "POST",
    headers: { ...auth, "x-chat2-frontier": "" },
    body: new Uint8Array([1, 2, 3])
  });
  if (seed.status !== 200) fail(`chat2 seed checkpoint: ${seed.status}`);
  else ok("chat2 room claimed via checkpoint POST");

  const payload = new Uint8Array([9, 9, 9, 9]);
  const push = await fetch(
    `${base}/chat2/${chatId}/rows?batchId=pp-row-1&device=${device}`,
    { method: "POST", headers: auth, body: payload }
  );
  const ack = await push.json();
  if (push.status !== 200 || ack.seq !== 1 || ack.dup !== false) {
    fail(`chat2 push: ${push.status} ${JSON.stringify(ack)}`);
  } else ok(`chat2 push acked seq=${ack.seq}`);

  const dup = await (
    await fetch(`${base}/chat2/${chatId}/rows?batchId=pp-row-1&device=${device}`, {
      method: "POST",
      headers: auth,
      body: payload
    })
  ).json();
  if (dup.dup !== true || dup.seq !== 1) fail(`chat2 push dedupe: ${JSON.stringify(dup)}`);
  else ok("chat2 push replay deduped");

  const pull = await fetch(`${base}/chat2/${chatId}/rows?after=0&device=pp-dev-b`, {
    headers: auth
  });
  if (pull.status !== 200) fail(`chat2 pull: ${pull.status}`);
  const frames = decodeFrames(await pull.arrayBuffer());
  const kinds = frames.map((f) => f.type);
  if (kinds[0] !== FRAME.state || kinds[kinds.length - 1] !== FRAME.rowsDone) {
    fail(`chat2 pull framing: kinds=${JSON.stringify(kinds)}`);
  } else ok(`chat2 pull framing state→rows→rowsDone (${frames.length} frames)`);
  const state = frames[0].header;
  if (state.headSeq !== 1 || state.checkpointSize !== 3) {
    fail(`chat2 pull state header: ${JSON.stringify(state)}`);
  } else ok(`chat2 pull state headSeq=${state.headSeq} checkpointSize=${state.checkpointSize}`);
  const row = frames.find((f) => f.type === FRAME.row);
  if (!row || row.header.seq !== 1 || row.header.batchId !== "pp-row-1" ||
      row.payload.length !== 4 || row.payload[0] !== 9) {
    fail(`chat2 pull row: ${row && JSON.stringify(row.header)}`);
  } else ok("chat2 pull row bytes intact");

  // exclude own rows
  const own = await fetch(
    `${base}/chat2/${chatId}/rows?after=0&device=${device}&excludeOwn=1`,
    { headers: auth }
  );
  const ownFrames = decodeFrames(await own.arrayBuffer());
  if (ownFrames.some((f) => f.type === FRAME.row)) fail("excludeOwn=1 still returned own rows");
  else ok("chat2 pull excludeOwn honored");
}

if (failures > 0) {
  console.error(`${failures} failure(s)`);
  process.exit(1);
}
console.log("ALL PULL/PUSH CHECKS PASSED");
