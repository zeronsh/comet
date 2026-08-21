#!/bin/sh
# Fake ACP agent for zeron-harness tests.
#
# Speaks scripted JSON-RPC 2.0 over stdio: initialize handshake, session
# new/load, then a scenario picked from the session/prompt text. Driven by
# crates/harness/tests/acp.rs. Always advertises the `_session/steering`
# extension; the steer-queue scenario REJECTS steer requests to exercise the
# turn-boundary fallback (the Grok-without-extension path).

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

update() { # $1 = update json object body
  emit "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"$SID\",\"update\":$1}}"
}

xnotify() { # $1 = update json object body — grok's extension channel (the
  # subagent lifecycle rides _x.ai/session_notification, NOT session/update;
  # same {sessionId, update} envelope. Verified live, 1.0.4.)
  emit "{\"method\":\"_x.ai/session_notification\",\"params\":{\"sessionId\":\"$SID\",\"update\":$1}}"
}

# ---- handshake -------------------------------------------------------------
read -r line || exit 1 # initialize
has "$line" '"method":"initialize"' || exit 1
has "$line" '"protocolVersion":1' || exit 1
has "$line" '"name":"zeron"' || exit 1
has "$line" '"readTextFile":false' || exit 1
emit "{\"id\":$(rid "$line"),\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true,\"_meta\":{\"availableCommands\":[{\"name\":\"compact\",\"description\":\"Compact the session\"},{\"name\":\"goal\",\"description\":\"Set a goal\",\"input\":{\"hint\":\"the goal\"}}]}},\"_meta\":{\"steering\":{\"supported\":true}}}}"

# ---- session new / load ----------------------------------------------------
read -r line || exit 1
SID="s-1"
MODEL_API=0
if has "$line" '"sessionId":"existing-grok-session"'; then
  MODEL_API=1
fi
if has "$line" '"method":"session/load"'; then
  if has "$line" '"sessionId":"load-fail"'; then
    emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32602,\"message\":\"unknown session\"}}"
    read -r line || exit 1
    has "$line" '"method":"session/new"' || exit 1
    emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"s-fresh\"}}"
    SID="s-fresh"
  else
    if [ "$MODEL_API" -eq 1 ]; then
      SID="existing-grok-session"
    else
      SID="s-loaded"
    fi
    # Replay history BEFORE the load response resolves: the harness must
    # drain these without emitting events (the doc already has them) and
    # without deadlocking on a full incoming channel.
    i=0
    while [ $i -lt 300 ]; do
      update '{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old prompt"}}'
      update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old reply"}}'
      i=$((i+1))
    done
    if [ "$MODEL_API" -eq 1 ]; then
      emit "{\"id\":$(rid "$line"),\"result\":{\"models\":{\"currentModelId\":\"grok-4-fast\",\"availableModels\":[{\"modelId\":\"grok-4-fast\",\"name\":\"Grok 4 Fast\"},{\"modelId\":\"grok-4.5\",\"name\":\"Grok 4.5\"}]}}}"
    else
      emit "{\"id\":$(rid "$line"),\"result\":{}}"
    fi
  fi
elif has "$line" '"method":"session/new"'; then
  has "$line" '"mcpServers":[]' || exit 1
  # Advertise config options: model (current differs from the tests' request,
  # forcing a set) and thought_level (current high). The model config option
  # feeds discovery first; the first-class `models` state (SessionModelState)
  # is the legacy fallback — codex-acp enumerates model × effort there.
  if [ "$MODEL_API" -eq 1 ]; then
    emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"s-1\",\"models\":{\"currentModelId\":\"grok-4-fast\",\"availableModels\":[{\"modelId\":\"grok-4-fast\",\"name\":\"Grok 4 Fast\"},{\"modelId\":\"grok-4.5\",\"name\":\"Grok 4.5\"}]}}}"
  else
    emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"s-1\",\"models\":{\"availableModels\":[{\"modelId\":\"grok-4-fast\",\"name\":\"Grok 4 Fast\",\"description\":\"Fast tier\"},{\"modelId\":\"grok-4.5\",\"name\":\"Grok 4.5\"}],\"currentModelId\":\"grok-4.5\"},\"configOptions\":[{\"id\":\"model\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"grok-4-fast\",\"options\":[{\"value\":\"grok-4-fast\",\"name\":\"Grok 4 Fast\",\"description\":\"Fast tier\"},{\"value\":\"grok-4.5\",\"name\":\"Grok 4.5\"}]},{\"id\":\"effort\",\"name\":\"Reasoning effort\",\"category\":\"thought_level\",\"type\":\"select\",\"currentValue\":\"high\",\"options\":[{\"value\":\"low\",\"name\":\"Low\"},{\"value\":\"medium\",\"name\":\"Medium\"},{\"value\":\"high\",\"name\":\"High\"}]}]}}"
  fi
else
  exit 1
fi

# ---- model/config sets (0..n), then the first turn ---------------------------
CONFIG_SETS=""
MODEL_SETS=""
read -r promptline || exit 1
while has "$promptline" '"method":"session/set_config_option"' \
  || has "$promptline" '"method":"session/set_model"'; do
  emit "{\"id\":$(rid "$promptline"),\"result\":{}}"
  if has "$promptline" '"method":"session/set_model"'; then
    MODEL_SETS="$MODEL_SETS $promptline"
  else
    CONFIG_SETS="$CONFIG_SETS $promptline"
  fi
  read -r promptline || exit 1
done
has "$promptline" '"method":"session/prompt"' || exit 1
pid=$(rid "$promptline")

case "$promptline" in

*scenario:model-api*)
  if has "$MODEL_SETS" '"modelId":"grok-4.5"'; then
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"model switched"}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:config*)
  # The tests' request carries model grok-4.5 + medium effort; both differ
  # from the advertised currents, so both sets must have arrived.
  if has "$CONFIG_SETS" '"configId":"model"' && has "$CONFIG_SETS" '"value":"grok-4.5"' \
    && has "$CONFIG_SETS" '"configId":"effort"' && has "$CONFIG_SETS" '"value":"medium"'; then
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"configured"}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:many*)
  # A wide read pass: 14 expandable chips — regression surface for the
  # analytic group-height accounting (auto-height cards clipped the tail).
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Reading the workspace."}}'
  i=1
  while [ $i -le 14 ]; do
    update "{\"sessionUpdate\":\"tool_call\",\"toolCallId\":\"r$i\",\"title\":\"read file $i\",\"kind\":\"read\",\"status\":\"completed\",\"rawInput\":{\"path\":\"/w/src/file_$i.rs\"},\"content\":[{\"type\":\"content\",\"content\":{\"type\":\"text\",\"text\":\"contents of file $i\"}}]}"
    i=$((i+1))
  done
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:prompt-complete-hang*)
  # The grok field hang: the turn really finishes — prompt_complete fires
  # with the echoed _meta.promptId — but the session/prompt RPC is NEVER
  # answered. The harness must settle off the notification.
  reqpid=$(printf '%s' "$promptline" | sed 's/.*"promptId":"\([^"]*\)".*/\1/')
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"pong"}}'
  emit "{\"method\":\"_x.ai/session/prompt_complete\",\"params\":{\"sessionId\":\"$SID\",\"promptId\":\"$reqpid\",\"stopReason\":\"end_turn\",\"agentResult\":null}}"
  # Hang the response forever (the wedge under test).
  exec sleep 60
  ;;

*scenario:prompt-complete-stale*)
  # A STALE completion (wrong prompt id — a replay of an already-settled
  # prompt) must not settle the live turn; the real response follows.
  emit "{\"method\":\"_x.ai/session/prompt_complete\",\"params\":{\"sessionId\":\"$SID\",\"promptId\":\"stale-p0\",\"stopReason\":\"cancelled\",\"agentResult\":null}}"
  # A completion for ANOTHER session is equally inert.
  emit "{\"method\":\"_x.ai/session/prompt_complete\",\"params\":{\"sessionId\":\"other\",\"promptId\":\"zeron-p1\",\"stopReason\":\"cancelled\",\"agentResult\":null}}"
  sleep 1
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"real answer"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",\"_meta\":{\"inputTokens\":9,\"outputTokens\":4}}}"
  ;;

*scenario:prompt-stall*)
  # Session boilerplate, then silence — the REAL wedge signature. opencode
  # emits available_commands_update on every session (provider dead or not),
  # so the watchdog must not count it as turn progress: exactly this frame
  # used to disarm the watchdog for the whole turn (found live, 1.18.18).
  emit "{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"$SID\",\"update\":{\"sessionUpdate\":\"available_commands_update\",\"availableCommands\":[{\"name\":\"init\",\"description\":\"guided AGENTS.md setup\"}]}}}"
  exec sleep 60
  ;;

*scenario:subagent*)
  # Grok's background subagent lifecycle (wire shapes captured live, 1.0.4):
  # spawn tool_call tagged _meta["x.ai/tool"], completion echoing the minted
  # subagent_id in its output text, then the subagent_spawned/finished
  # extension updates. The transcript itself never rides the wire — the test
  # seeds/app ends a chat_history.jsonl under the harness's sessions root
  # and the tail turns it into tagged events during the sleep window.
  update '{"sessionUpdate":"tool_call","toolCallId":"sp1","title":"spawn_subagent","rawInput":{"description":"Count files","prompt":"Count the files.","subagent_type":"explore"},"_meta":{"x.ai/tool":{"version":1,"name":"spawn_subagent","kind":"task","namespace":"grok_build","label":"Subagent","read_only":false},"subagentBackground":true}}'
  update '{"sessionUpdate":"tool_call_update","toolCallId":"sp1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"Subagent started in background.\nsubagent_id: sub-1\ntype: explore"}}],"rawOutput":{"type":"Text","text":"Subagent started in background.\nsubagent_id: sub-1\ntype: explore"}}'
  xnotify "{\"sessionUpdate\":\"subagent_spawned\",\"subagent_id\":\"sub-1\",\"parent_session_id\":\"$SID\",\"child_session_id\":\"sub-1\",\"subagent_type\":\"explore\",\"description\":\"Count files\"}"
  # A NESTED spawned update (another parent session) must not bind here.
  xnotify '{"sessionUpdate":"subagent_spawned","subagent_id":"sub-nested","parent_session_id":"sub-1","child_session_id":"sub-nested","subagent_type":"explore","description":"Count files"}'
  sleep 1.4
  xnotify '{"sessionUpdate":"subagent_finished","subagent_id":"sub-1","child_session_id":"sub-1","status":"completed","tool_calls":1,"turns":1,"output":"two files","will_wake":false}'
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:happy*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello"}}'
  update '{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}'
  # Non-text content chunks map to nothing.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"image","data":"x","mimeType":"image/png"}}'
  # Execute tool: pending call, then completed update with output content.
  update '{"sessionUpdate":"tool_call","toolCallId":"t1","title":"cargo test -p zeron-harness","kind":"execute","status":"pending","rawInput":{"command":"cargo test -p zeron-harness"}}'
  update '{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"   Compiling zeron-harness v0.1.21\n    Finished `dev` profile [unoptimized] in 2.41s\n     Running tests/acp.rs\n\nrunning 13 tests\ntest result: ok. 13 passed; 0 failed; 0 ignored"}}]}'
  # Edit tool resolved in one shot with an inline diff (real hunk: context,
  # line numbers, rust syntax for the transcript's diff component).
  update '{"sessionUpdate":"tool_call","toolCallId":"t2","title":"edit resolve.rs","kind":"edit","status":"completed","content":[{"type":"diff","path":"/w/src/resolve.rs","oldText":"use std::path::PathBuf;\n\n/// Locate the agent binary.\nfn resolve(exe: &str) -> Option<PathBuf> {\n    std::env::var_os(\"PATH\")\n        .map(PathBuf::from)\n        .filter(|p| p.exists())\n}\n","newText":"use std::path::PathBuf;\n\n/// Locate the agent binary.\nfn resolve(exe: &str) -> Option<PathBuf> {\n    let dirs = std::env::split_paths(&std::env::var_os(\"PATH\")?);\n    dirs.map(|d| d.join(exe)).find(|p| p.exists())\n}\n"}]}'
  # Plan → todo chip.
  update '{"sessionUpdate":"plan","entries":[{"content":"read","priority":"high","status":"completed"},{"content":"fix","priority":"high","status":"in_progress"}]}'
  # Command advertisement mid-run.
  update '{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"deep-research","description":"Research deeply"}]}'
  # Context gauge + unknown kinds are tolerated, another session filtered.
  update '{"sessionUpdate":"usage_update","used":1200,"size":500000}'
  update '{"sessionUpdate":"totally_unknown_kind","x":1}'
  emit '{"method":"session/update","params":{"sessionId":"other","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"WRONG SESSION"}}}}'
  emit '{"method":"some/unknownNotification","params":{"x":1}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:echo-prompt*)
  # Echo the prompt's first text block back as a delta (prompt-transform
  # verification: Ultrathink prefix must be on the wire).
  text=$(printf '%s' "$promptline" | sed 's/.*"text":"\([^"]*\)".*/\1/')
  update "{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"$text\"}}"
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:question*)
  # AskUserQuestion-shaped request: options WITHOUT allow/reject kinds are
  # user-facing choices — must round-trip through the input bridge, never
  # auto-accept. The test's bridge answers "Use tokio".
  emit "{\"id\":88,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"$SID\",\"toolCall\":{\"toolCallId\":\"q1\",\"title\":\"Which async runtime should I use?\"},\"options\":[{\"optionId\":\"opt-tokio\",\"name\":\"Use tokio\"},{\"optionId\":\"opt-smol\",\"name\":\"Use smol\"}]}}"
  read -r ans || exit 1
  { has "$ans" '"id":88' && has "$ans" '"outcome":"selected"' && has "$ans" '"optionId":"opt-tokio"'; } ||
    { emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"; exit 0; }
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answered"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:permission*)
  emit "{\"id\":77,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"$SID\",\"toolCall\":{\"toolCallId\":\"t1\"},\"options\":[{\"optionId\":\"once\",\"name\":\"Allow once\",\"kind\":\"allow_once\"},{\"optionId\":\"always\",\"name\":\"Always allow\",\"kind\":\"allow_always\"},{\"optionId\":\"no\",\"name\":\"Reject\",\"kind\":\"reject_once\"}]}}"
  read -r ans || exit 1
  { has "$ans" '"id":77' && has "$ans" '"outcome":"selected"' && has "$ans" '"optionId":"always"'; } ||
    { emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"; exit 0; }
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"approved"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:steer-ext*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  if has "$steerline" '"method":"_session/steering"' &&
    has "$steerline" 'redirect please' &&
    has "$steerline" '"idleBehavior":"promptRequired"'; then
    emit "{\"id\":$sid,\"result\":{\"outcome\":\"injected\"}}"
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"steered"}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$sid,\"error\":{\"code\":-32600,\"message\":\"bad steer\"}}"
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:steer-race*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  has "$steerline" '"method":"_session/steering"' || exit 1
  # The exact turn-end race: the injection lands in the turn's tail and the
  # PROMPT response hits the wire before the steering response does. The
  # harness must settle the steering call at the boundary — Steered before
  # Done, never after — instead of stranding the session or re-prompting.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"steered"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  sleep 0.2
  emit "{\"id\":$sid,\"result\":{\"outcome\":\"injected\"}}"
  ;;

*scenario:steer-queue*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  has "$steerline" '"method":"_session/steering"' || exit 1
  # Reject: the harness must queue the text and deliver it as the next
  # session/prompt at the turn boundary (the no-extension path).
  emit "{\"id\":$sid,\"error\":{\"code\":-32601,\"message\":\"steering unsupported\"}}"
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  read -r followline || exit 1
  fid=$(rid "$followline")
  if has "$followline" '"method":"session/prompt"' && has "$followline" 'redirect please'; then
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"boundary"}}'
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:starve*)
  # The 2026-08-12 wedge: the prompt's turn was consumed by CLI-side
  # self-continuation and its response NEVER comes. Steering then answers
  # noRunningTurn — the harness must settle the dead turn after its grace
  # and promote the queued steer to a fresh session/prompt.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  has "$steerline" '"method":"_session/steering"' || exit 1
  emit "{\"id\":$sid,\"result\":{\"outcome\":\"promptRequired\",\"reason\":\"noRunningTurn\"}}"
  # No response to $pid, ever. The next line must be the promoted prompt.
  read -r followline || exit 1
  fid=$(rid "$followline")
  if has "$followline" '"method":"session/prompt"' && has "$followline" 'what about now'; then
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"promoted"}}'
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:cost-starve*)
  # The dropped-reply turn end, no steer involved: the turn's terminal
  # cost frame arrives but the prompt response never does. The harness
  # (claude spec) must settle the turn ~1s after the cost frame instead of
  # stranding Working. Script exits shortly after so the stream ends.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  update '{"sessionUpdate":"usage_update","used":22457,"size":1000000,"cost":{"amount":0.01,"currency":"USD"}}'
  # No response to $pid, ever.
  sleep 6
  exit 0
  ;;

*scenario:busy-steer*)
  # Prevention: the turn settles, then the agent SELF-CONTINUES (a turn no
  # prompt started, visible as an open tool call). A steer arriving now
  # must NOT become a session/prompt (the adapter drops that reply — the
  # verified starve): the harness cancels the unowned turn first, then
  # prompts after the flush window.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  update '{"sessionUpdate":"tool_call","toolCallId":"sc-1","title":"self-continued work","kind":"execute","status":"pending","rawInput":{"command":"make"}}'
  read -r cancelline || exit 1
  has "$cancelline" '"method":"session/cancel"' || exit 1
  update '{"sessionUpdate":"tool_call_update","toolCallId":"sc-1","status":"completed","content":[]}'
  read -r followline || exit 1
  fid=$(rid "$followline")
  if has "$followline" '"method":"session/prompt"' && has "$followline" 'what about now'; then
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"fresh answer"}}'
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:native-busy-steer*)
  # Claude's native path: steer into a self-continued turn must arrive as a
  # plain session/prompt (NO cancel — the CLI folds it into the running
  # turn natively). The adapter drops that prompt's reply; the harness must
  # settle off the turn-end cost frame instead.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  update '{"sessionUpdate":"tool_call","toolCallId":"sc-2","title":"self-continued work","kind":"execute","status":"pending","rawInput":{"command":"make"}}'
  read -r followline || exit 1
  if has "$followline" '"method":"session/cancel"'; then
    # Cancelling would kill the agent's in-flight work: fail loudly.
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"CANCELLED-NATIVE-WORK"}}'
    exit 1
  fi
  fid=$(rid "$followline")
  { has "$followline" '"method":"session/prompt"' && has "$followline" 'what about now'; } || exit 1
  # The merged turn finishes: tool resolves, folded reply streams, the
  # terminal cost frame arrives — and the prompt response NEVER does.
  update '{"sessionUpdate":"tool_call_update","toolCallId":"sc-2","status":"completed","content":[]}'
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"merged reply"}}'
  update '{"sessionUpdate":"usage_update","used":30000,"size":1000000,"cost":{"amount":0.02,"currency":"USD"}}'
  sleep 6
  exit 0
  ;;

*scenario:steer-cost-noise*)
  # The injection cost frame (2026-08-13): claude-agent-acp stamps a
  # cost-bearing usage_update for the injected message itself, MID-turn,
  # identical in shape to the terminal one. The harness (claude spec) must
  # not settle off it — the turn continues and ends via its real response:
  # exactly one Done, all text intact.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  has "$steerline" '"method":"_session/steering"' || exit 1
  emit "{\"id\":$sid,\"result\":{\"outcome\":\"injected\"}}"
  update '{"sessionUpdate":"usage_update","used":21429,"size":200000,"cost":{"amount":0.0006,"currency":"USD"},"_meta":{"_claude/origin":{"kind":"human"}}}'
  sleep 3
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"steered tail"}}'
  update '{"sessionUpdate":"usage_update","used":21884,"size":200000,"cost":{"amount":0.02,"currency":"USD"},"_meta":{"_claude/origin":{"kind":"human"}}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:quiet-starve*)
  # Blanket dropped-reply settle, no adapter-specific evidence: content
  # streamed, no open tool, then silence — the response never comes. The
  # harness must settle off the generic quiet window (tests set
  # ZERON_ACP_QUIET_SETTLE_MS small), well before this stream's 8s EOF.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  sleep 8
  exit 0
  ;;

*scenario:quiet-tool-guard*)
  # The guard: an OPEN tool call makes silence legitimate. Quiet stretch is
  # far past the test's settle window, but the pending tool must hold the
  # settle off; the turn then ends normally — exactly one Done.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  update '{"sessionUpdate":"tool_call","toolCallId":"slow-1","title":"slow build","kind":"execute","status":"pending","rawInput":{"command":"make"}}'
  sleep 4
  update '{"sessionUpdate":"tool_call_update","toolCallId":"slow-1","status":"completed","content":[]}'
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"finished"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:quiet-thinking*)
  # The 2026-08-13 false settle: every tool RESOLVED, then a long silent
  # thinking stretch (claude-agent-acp forwards no thinking traffic), then
  # the turn continues and ends normally. This is exactly the "looks
  # finished" state the blanket settle keys on; Claude must hold through
  # it — a false settle here orphans the real turn (its response lands on
  # a closed channel; the session strands Working).
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  update '{"sessionUpdate":"tool_call","toolCallId":"th-1","title":"quick read","kind":"read","status":"pending","rawInput":{"path":"/w/src/x.rs"}}'
  update '{"sessionUpdate":"tool_call_update","toolCallId":"th-1","status":"completed","content":[]}'
  sleep 4
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"finished"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:interrupt*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  read -r intline || exit 1
  if has "$intline" '"method":"session/cancel"'; then
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"cancelled\"}}"
  else
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:wedge*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  # Ignore session/cancel entirely — forces the SIGTERM escalation path.
  exec sleep 30
  ;;

*scenario:refusal*)
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  ;;

*scenario:resumed*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"back again"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*)
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  ;;
esac
