#!/bin/sh
# Fake `hermes acp` server for comet-harness tests.
#
# Speaks scripted ACP (JSON-RPC 2.0 over stdio): initialize handshake,
# session/new or session/load, the set_model/set_mode preamble, then a scenario
# picked from the session/prompt text. Every notification shape below is copied
# from a live `hermes acp` 0.19.1 capture. Driven by
# crates/harness/tests/hermes.rs.

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

SID='s-live'

# `update` notification for session $SID.
upd() { emit "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"$SID\",\"update\":$1}}"; }

msg() { upd "{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":$1}}"; }

# ---- handshake -------------------------------------------------------------
read -r line || exit 1
has "$line" '"method":"initialize"' || exit 1
has "$line" '"name":"comet-native"' || exit 1
# Comet must NOT claim a client filesystem it does not serve.
has "$line" '"readTextFile":false' || exit 1
emit "{\"id\":$(rid "$line"),\"result\":{\"protocolVersion\":1,\"agentInfo\":{\"name\":\"hermes-agent\",\"version\":\"0.19.1\"},\"agentCapabilities\":{\"loadSession\":true,\"promptCapabilities\":{\"image\":true}}}}"

# The model catalog + modes both session/new and session/load return.
MODELS='"models":{"currentModelId":"xai-oauth:grok-4.5","availableModels":[{"modelId":"xai-oauth:grok-4.5","name":"xAI · grok-4.5","description":"Provider: xAI"},{"modelId":"openai-codex:gpt-5.5","name":"OpenAI Codex · gpt-5.5"}]}'
MODES='"modes":{"currentModeId":"default","availableModes":[{"id":"default","name":"Default"},{"id":"accept_edits","name":"Accept Edits"},{"id":"dont_ask","name":"Don'"'"'t Ask"}]}'

# ---- session/new | session/load --------------------------------------------
read -r line || exit 1
if has "$line" '"method":"session/load"'; then
  if has "$line" '"sessionId":"resume-fail"'; then
    # Hermes returns a null result for a session it cannot find; the harness
    # must fall back to session/new.
    emit "{\"id\":$(rid "$line"),\"result\":null}"
    read -r line || exit 1
    has "$line" '"method":"session/new"' || exit 1
    SID='s-fresh'
    emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"$SID\",$MODELS,$MODES}}"
  else
    # A real load REPLAYS the prior transcript as session/update notifications
    # BEFORE responding. Comet's doc already holds these; none may reach the
    # event stream.
    SID='s-resumed'
    upd '{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"replayed user turn"}}'
    msg '"REPLAYED-ASSISTANT-TEXT"'
    upd '{"sessionUpdate":"tool_call","toolCallId":"tc-replay","kind":"read","title":"read: old.txt","locations":[{"path":"old.txt"}]}'
    emit "{\"id\":$(rid "$line"),\"result\":{$MODELS,$MODES}}"
  fi
elif has "$line" '"method":"session/new"'; then
  emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"$SID\",$MODELS,$MODES}}"
else
  exit 1
fi

# ---- set_model / set_mode preamble, then the turn --------------------------
SAW_SET_MODEL=no
SAW_MODE=''
SAW_REASONING=''
SAW_SERVICE_TIER=''
promptline=''
while read -r line; do
  case "$line" in
  *'"method":"session/set_model"'*)
    SAW_SET_MODEL=yes
    has "$line" '"modelId":"openai-codex:gpt-5.5"' || exit 1
    emit "{\"id\":$(rid "$line"),\"result\":{}}"
    ;;
  *'"method":"session/set_mode"'*)
    SAW_MODE=$(printf '%s' "$line" | sed 's/.*"modeId":"\([a-z_]*\)".*/\1/')
    emit "{\"id\":$(rid "$line"),\"result\":{}}"
    ;;
  *'"method":"session/set_config_option"'*'"configId":"reasoning_effort"'*)
    SAW_REASONING=$(printf '%s' "$line" | sed 's/.*"value":"\([a-z]*\)".*/\1/')
    emit "{\"id\":$(rid "$line"),\"result\":{\"configOptions\":[]}}"
    ;;
  *'"method":"session/set_config_option"'*'"configId":"service_tier"'*)
    SAW_SERVICE_TIER=$(printf '%s' "$line" | sed 's/.*"value":"\([a-z]*\)".*/\1/')
    emit "{\"id\":$(rid "$line"),\"result\":{\"configOptions\":[]}}"
    ;;
  *'"method":"session/prompt"'*)
    promptline="$line"
    break
    ;;
  *) : ;;
  esac
done
[ -n "$promptline" ] || exit 1
pid=$(rid "$promptline")

USAGE='"usage":{"inputTokens":16165,"outputTokens":20,"thoughtTokens":14,"totalTokens":16185,"cachedReadTokens":2432}'

case "$promptline" in

*scenario:happy*)
  # The preamble the harness must have sent before the turn.
  [ "$SAW_SET_MODEL" = yes ] || exit 1
  # auto_approve=true must pick Hermes's most permissive edit policy.
  [ "$SAW_MODE" = dont_ask ] || exit 1
  has "$promptline" '"type":"text"' || exit 1

  upd '{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"help"}]}'
  upd '{"sessionUpdate":"usage_update","size":500000,"used":10972}'
  upd '{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}'
  msg '"Hello"'
  msg '" world"'
  # Live tool_call frames (terminal / read / write).
  upd '{"sessionUpdate":"tool_call","toolCallId":"tc-1","kind":"execute","locations":[],"title":"terminal: ls -la","content":[{"type":"content","content":{"type":"text","text":"$ ls -la"}}]}'
  upd '{"sessionUpdate":"tool_call_update","toolCallId":"tc-1","kind":"execute","title":"terminal: ls -la","status":"completed"}'
  upd '{"sessionUpdate":"tool_call","toolCallId":"tc-2","kind":"read","locations":[{"path":"notes.txt"}],"title":"read: notes.txt"}'
  upd '{"sessionUpdate":"tool_call_update","toolCallId":"tc-2","kind":"read","locations":[{"path":"notes.txt"}],"title":"read: notes.txt","status":"failed"}'
  # A progress-only update resolves nothing.
  upd '{"sessionUpdate":"tool_call_update","toolCallId":"tc-3","kind":"edit","title":"write: out.txt","locations":[{"path":"out.txt"}],"status":"in_progress"}'
  # MCP call: prefixed name + rawInput.
  upd '{"sessionUpdate":"tool_call","toolCallId":"tc-4","kind":"other","title":"mcp__linear__create_issue","rawInput":{"team":"eng"}}'
  upd '{"sessionUpdate":"plan","entries":[{"content":"Read the code","status":"completed"},{"content":"Write the fix","status":"pending"}]}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",$USAGE}}"
  ;;

*scenario:traits*)
  [ "$SAW_REASONING" = high ] || exit 1
  [ "$SAW_SERVICE_TIER" = priority ] || exit 1
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:resumed*)
  msg '"after resume"'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",$USAGE}}"
  ;;

*scenario:readonly*)
  # Sandbox ReadOnly without auto_approve maps to Hermes's "default" mode.
  [ "$SAW_MODE" = default ] || exit 1
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:permission*)
  upd '{"sessionUpdate":"tool_call","toolCallId":"tc-p","kind":"edit","title":"write: notes.txt","locations":[{"path":"notes.txt"}]}'
  # Server→client permission request, exactly as captured live.
  emit "{\"id\":900,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"$SID\",\"options\":[{\"kind\":\"allow_once\",\"name\":\"Allow edit\",\"optionId\":\"allow_once\"},{\"kind\":\"reject_once\",\"name\":\"Deny\",\"optionId\":\"deny\"}],\"toolCall\":{\"toolCallId\":\"edit-approval-1\",\"status\":\"pending\",\"kind\":\"edit\",\"title\":\"Approve edit: notes.txt\"}}}"
  read -r reply || exit 1
  # Echo the option the harness picked so the test can assert on it.
  picked=$(printf '%s' "$reply" | sed 's/.*"optionId":"\([a-z_]*\)".*/\1/')
  msg "\"picked:$picked\""
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:steer*)
  msg '"first"'
  # Block until the harness forwards the steer as a second session/prompt.
  read -r steerline || exit 1
  has "$steerline" '"method":"session/prompt"' || exit 1
  has "$steerline" 'steered text' || exit 1
  # Hermes acks the redirect immediately (no usage) and streams the ack as
  # ordinary assistant text — which Comet must swallow.
  msg '"Redirected the active turn with your correction."'
  emit "{\"id\":$(rid "$steerline"),\"result\":{\"stopReason\":\"end_turn\"}}"
  msg '"second"'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",$USAGE}}"
  ;;

*scenario:interrupt*)
  msg '"working"'
  # Wait for session/cancel (a notification — no response), then resolve the
  # pending prompt as cancelled.
  read -r cancelline || exit 1
  has "$cancelline" '"method":"session/cancel"' || exit 1
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"cancelled\"}}"
  ;;

*scenario:promptfail*)
  emit "{\"id\":$pid,\"error\":{\"code\":-32603,\"message\":\"provider exploded\"}}"
  ;;

*)
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;
esac

# Stay alive for the persistent-session steering mailbox until stdin closes.
while read -r line; do
  case "$line" in
  *'"method":"session/prompt"'*)
    nid=$(rid "$line")
    msg '"follow-up turn"'
    emit "{\"id\":$nid,\"result\":{\"stopReason\":\"end_turn\"}}"
    ;;
  *) : ;;
  esac
done
exit 0
