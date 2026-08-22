// zeron cursor shim — a thin, zeron-owned wrapper around the PINNED
// @cursor/sdk (see CURSOR_SDK_PIN in crates/harness/src/cursor/mod.rs; the
// SDK is public beta with ~weekly releases — expect churn, revalidate on
// every bump). Materialized by zeron into the SDK's managed install dir so
// `import("@cursor/sdk")` resolves from the sibling node_modules.
//
// Why a shim at all: @cursor/sdk does NOT wrap the cursor-agent binary — it
// is Cursor's agent runtime bundled in-process, speaking proprietary
// protobuf/ConnectRPC to Cursor's backend, with client-side tool execution.
// There is no stdio wire to drive from Rust; this file IS the wire.
//
// Protocol: JSONL, one frame per line.
//   stdin  (engine → shim):
//     {"op":"run","prompt","cwd","model"?,"modelOptions"?,"resume"?}   start / first turn
//     {"op":"user","prompt"}                            next turn (parked)
//     {"op":"interrupt"}                                cancel the live run
//   stdout (shim → engine):
//     {"ev":"ready","agentId","model"?}
//     {"ev":"text","text","parent"?}        parent = spawning task callId
//     {"ev":"thinking","text","parent"?}
//     {"ev":"tool","phase":"start"|"end","id","name","args"?,"error"?,"parent"?}
//     {"ev":"usage","input","output"}
//     {"ev":"turn","status":"finished"|"error"|"cancelled","error"?}
//     {"ev":"fatal","message"}              unrecoverable (auth, SDK init)
//
// Models mode (`node <shim> models`): prints {"ev":"models","items":[…]} —
// the live `Cursor.models.list()` catalog — and exits.
//
// Login mode (`node <shim> login <store-path>`): no stdin protocol — runs the
// SDK's PKCE browser flow (`Cursor.auth.login`) writing the minted key to
// <store-path> instead of the live `~/.cursor/sdk/auth.json` (the engine
// snapshots it as an account slot, mirroring codex's throwaway CODEX_HOME).
//   stdout: {"ev":"auth-url","url"}         open/show this to the user
//           {"ev":"logged-in","email"?,"expiresAtMs"?}   then exit 0
//           {"ev":"fatal","message"}                     then exit 1
//
// Unknown SDK update kinds are ignored (never fatal): the SDK is beta and
// its delta union grows; the engine treats a degraded stream as chip-only.

import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";

const out = (o) => process.stdout.write(JSON.stringify(o) + "\n");
const fatal = (message) => {
  out({ ev: "fatal", message: String(message) });
  process.exit(1);
};

let sdk;
try {
  sdk = await import("@cursor/sdk");
} catch (e) {
  fatal(`@cursor/sdk failed to load: ${e?.message ?? e}`);
}
const { Agent, Cursor, FileCredentialStore, JsonlLocalAgentStore } = sdk;

// ---- per-run agent store --------------------------------------------------
// The SDK's default local store is SQLite keyed by WORKSPACE
// (`getDefaultSdkStateRoot(cwd)`), designed for one process per workspace.
// Zeron runs concurrent shims on the same cwd as a matter of course — the
// chat turn and its title generation start together, and two chats can share
// a worktree — and the second `Agent.create` dies with "database is locked"
// (reproduced live, 1.0.28). The shared-root JSONL backend is no better: its
// writes are only serialized process-WIDE and updates rewrite whole files.
//
// So each run gets its OWN JsonlLocalAgentStore directory (one agent per
// store — the "huge catalog" caveat never applies), and a one-writer marker
// file maps agentId → store dir so `Agent.resume` from a later process finds
// it. Agents created before this scheme have no marker and fall back to the
// SDK default store, which is where they live.
const STATE_BASE =
  process.env.ZERON_CURSOR_STATE_DIR || path.join(os.homedir(), ".zeron", "cursor-state");

function agentDirMarker(agentId) {
  return path.join(STATE_BASE, "by-agent", String(agentId));
}

function newRunStore() {
  const dir = path.join(STATE_BASE, "agents", crypto.randomUUID());
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  return { dir, store: new JsonlLocalAgentStore(dir) };
}

function storeForResume(agentId) {
  try {
    const dir = fs.readFileSync(agentDirMarker(agentId), "utf8").trim();
    if (dir && fs.existsSync(dir)) return { dir, store: new JsonlLocalAgentStore(dir) };
  } catch {
    // No marker: a pre-isolation agent living in the SDK's default store.
  }
  return null;
}

function rememberAgentDir(agentId, dir) {
  try {
    const marker = agentDirMarker(agentId);
    fs.mkdirSync(path.dirname(marker), { recursive: true, mode: 0o700 });
    const tmp = `${marker}.tmp-${process.pid}`;
    fs.writeFileSync(tmp, dir);
    fs.renameSync(tmp, marker);
  } catch {
    // Fail soft: the run still works; only a future resume degrades to the
    // default store (and surfaces as a fresh session, not a crash).
  }
}

// ---- models mode ----------------------------------------------------------
// `node <shim> models`: print the live catalog (`Cursor.models.list()` — no
// auth required, verified live on 1.0.28) as one frame and exit. The harness
// maps items (id/displayName/parameters/variants) into its picker models.
if (process.argv[2] === "models") {
  try {
    const listed = await Cursor.models.list();
    const items = Array.isArray(listed) ? listed : (listed?.items ?? []);
    out({ ev: "models", items });
    process.exit(0);
  } catch (e) {
    fatal(`cursor model discovery failed: ${e?.message ?? e}`);
  }
}

// ---- login mode -----------------------------------------------------------
// The browser flow never opens a browser itself (the engine may be headless;
// the URL can be opened on whichever device is viewing the app), and the
// minted key lands in the engine-chosen store file, never the live login.
if (process.argv[2] === "login") {
  const storePath = process.argv[3];
  if (!storePath) fatal("login mode needs a store path");
  try {
    const result = await Cursor.auth.login({
      openBrowser: false,
      onLoginUrl: (url) => out({ ev: "auth-url", url }),
      store: new FileCredentialStore(storePath),
      apiKeyName: `zeron — ${os.hostname()}`,
    });
    out({
      ev: "logged-in",
      ...(result?.email ? { email: result.email } : {}),
      ...(result?.apiKeyExpiresAtMs ? { expiresAtMs: result.apiKeyExpiresAtMs } : {}),
    });
    process.exit(0);
  } catch (e) {
    fatal(`cursor login failed: ${e?.message ?? e}`);
  }
}

let agent = null;
let run = null;
let interrupted = false;

// One InteractionUpdate → zero or one frame. `parent` attributes nested
// subagent traffic (tool-call-delta carries the child's updates tagged by
// the spawning task's callId — the full subagent transcript, live).
function mapUpdate(u, parent) {
  if (!u || typeof u !== "object") return;
  const tag = parent ? { parent } : {};
  switch (u.type) {
    case "text-delta":
      if (u.text) out({ ev: "text", text: u.text, ...tag });
      break;
    case "thinking-delta":
      if (u.text) out({ ev: "thinking", text: u.text, ...tag });
      break;
    case "tool-call-started":
      out({
        ev: "tool",
        phase: "start",
        id: u.callId,
        name: u.toolCall?.type ?? "tool",
        args: u.toolCall?.args ?? null,
        ...tag,
      });
      break;
    case "tool-call-completed": {
      const r = u.toolCall?.result;
      const failed =
        r?.status === "error" ||
        (r?.status === "success" &&
          typeof r?.value?.exitCode === "number" &&
          r.value.exitCode !== 0);
      out({
        ev: "tool",
        phase: "end",
        id: u.callId,
        name: u.toolCall?.type ?? "tool",
        args: u.toolCall?.args ?? null,
        error: Boolean(failed),
        ...tag,
      });
      break;
    }
    case "tool-call-delta":
      // Nested subagent update, tagged by the spawning task call.
      if (u.taskUpdate) mapUpdate(u.taskUpdate, u.callId);
      break;
    case "turn-ended":
      if (u.usage) {
        out({
          ev: "usage",
          input: u.usage.inputTokens ?? 0,
          output: u.usage.outputTokens ?? 0,
        });
      }
      break;
    default:
      // step-*/summary-*/token-delta/partial-tool-call/…: no consumer.
      break;
  }
}

// Auth errors surface at RUN time (Agent.create succeeds unauthed —
// verified live: the turn fails with "[unknown] Invalid User API Key").
// Attach the exact fix, because the SDK's credentials are separate from
// `cursor-agent login`.
function withAuthHint(message) {
  const text = String(message ?? "");
  if (/api key|not authenticated|unauthorized/i.test(text)) {
    return (
      text +
      " — connect Cursor in Settings → Accounts (its login is separate from " +
      "`cursor-agent login`), or set CURSOR_API_KEY from cursor.com/settings."
    );
  }
  return text;
}

async function runTurn(prompt) {
  interrupted = false;
  try {
    run = await agent.send(prompt, {
      onDelta: ({ update }) => {
        try {
          mapUpdate(update);
        } catch {
          // A malformed update must never kill the turn.
        }
      },
    });
  } catch (e) {
    out({ ev: "turn", status: "error", error: withAuthHint(e?.message ?? e) });
    run = null;
    return;
  }
  let result;
  try {
    result = await run.wait();
  } catch (e) {
    out({
      ev: "turn",
      status: interrupted ? "cancelled" : "error",
      error: withAuthHint(e?.message ?? e),
    });
    run = null;
    return;
  }
  run = null;
  out({
    ev: "turn",
    status: result?.status ?? "finished",
    ...(result?.error?.message ? { error: withAuthHint(result.error.message) } : {}),
  });
}

// The run's ModelSelection: id + typed parameter values (thinking / context /
// effort / fast / optimize_for — ids from the discovered catalog). Legacy
// sessions may still carry the pre-discovery static option ("optimizeFor",
// choice "speed") — translate rather than send an id the backend never knew.
function modelSelection(msg) {
  const id = msg.model || "auto";
  const params = [];
  for (const [key, value] of Object.entries(msg.modelOptions ?? {})) {
    if (typeof value !== "string" || !value) continue;
    if (key === "optimizeFor") {
      params.push({ id: "optimize_for", value: value === "speed" ? "cost" : value });
    } else {
      params.push({ id: key, value });
    }
  }
  return params.length ? { id, params } : { id };
}

async function start(msg) {
  const model = modelSelection(msg);
  const local = { cwd: msg.cwd || process.cwd(), enableAgentRetries: true };
  // Isolated per-run store (see the header above). Resume looks the agent's
  // store up by marker; a markerless (pre-isolation) agent resumes from the
  // SDK's default store exactly as before.
  let runDir = null;
  if (msg.resume) {
    const found = storeForResume(msg.resume);
    if (found) {
      local.store = found.store;
      runDir = found.dir;
    }
  } else {
    const fresh = newRunStore();
    local.store = fresh.store;
    runDir = fresh.dir;
  }
  const options = {
    model,
    // askQuestion has no public answer channel in this SDK (SDKRequestMessage
    // carries only a request id) — a question would block the run forever.
    // generateImage has nowhere to land in a zeron session (ACP parity).
    disallowedTools: ["askQuestion", "generateImage"],
    local,
  };
  try {
    agent = msg.resume
      ? await Agent.resume(msg.resume, options)
      : await Agent.create(options);
  } catch (e) {
    // Auth is the common cause: the SDK's credentials are SEPARATE from
    // `cursor-agent login` (verified) — name the fix precisely.
    const auth = await Cursor.auth.status().catch(() => null);
    if (!process.env.CURSOR_API_KEY && auth?.status !== "logged-in") {
      fatal(
        "Cursor is not connected (its login is separate from " +
          "`cursor-agent login`): connect it in Settings → Accounts, or set " +
          `CURSOR_API_KEY from cursor.com/settings, then retry. (${e?.message ?? e})`,
      );
    }
    fatal(`cursor agent failed to start: ${e?.message ?? e}`);
  }
  if (runDir) rememberAgentDir(agent.agentId, runDir);
  out({ ev: "ready", agentId: agent.agentId, model: agent.model?.id ?? model.id });
  await runTurn(msg.prompt ?? "");
}

const rl = readline.createInterface({ input: process.stdin });
let chain = Promise.resolve();
rl.on("line", (line) => {
  line = line.trim();
  if (!line) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  switch (msg.op) {
    case "run":
      chain = chain.then(() => start(msg)).catch((e) => fatal(e));
      break;
    case "user":
      chain = chain
        .then(() => (agent ? runTurn(msg.prompt ?? "") : undefined))
        .catch((e) => fatal(e));
      break;
    case "interrupt":
      interrupted = true;
      if (run) run.cancel().catch(() => {});
      break;
    default:
      break;
  }
});
rl.on("close", () => {
  // stdin EOF: the engine is done with the session.
  const r = run;
  run = null;
  (r ? r.cancel().catch(() => {}) : Promise.resolve()).finally(() => {
    try {
      agent?.close();
    } catch {}
    process.exit(0);
  });
});
