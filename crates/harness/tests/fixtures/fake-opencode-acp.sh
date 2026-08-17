#!/bin/sh
# Fake opencode ACP server for zeron-harness tests.
#
# Mimes the real `opencode acp` wire: initialize with NO `_session/steering`
# extension (opencode delivers at turn boundaries), then session/new
# advertising `model` + `mode` config options and NO thought_level option
# (opencode exposes no effort ladder). Driven by crates/harness/tests/acp.rs.

# The spec launches with `opencode acp`; refuse anything else.
[ "$1" = "acp" ] || exit 1

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

update() { # $1 = update json object body
  emit "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"$SID\",\"update\":$1}}"
}

# ---- handshake -------------------------------------------------------------
read -r line || exit 1 # initialize
has "$line" '"method":"initialize"' || exit 1
has "$line" '"protocolVersion":1' || exit 1
has "$line" '"name":"zeron"' || exit 1
has "$line" '"readTextFile":false' || exit 1
# No `_meta.steering` — opencode has no steering extension; steers arrive as
# plain session/prompt at turn boundaries.
emit "{\"id\":$(rid "$line"),\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true,\"_meta\":{\"availableCommands\":[{\"name\":\"compact\",\"description\":\"Compact the session\"}]}}}}"

# ---- session new -----------------------------------------------------------
read -r line || exit 1
SID="s-op"
if has "$line" '"method":"session/new"'; then
  has "$line" '"mcpServers":[]' || exit 1
  # opencode's advertised state: model select (OpenCode Zen, current differs
  # from the probe's default to force a set) + mode select (build/plan), no
  # thought_level. The mode option rides as a Traits select only when the run
  # selects it; opencode's own defaults match zeron's (build, no prompts).
  emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"s-op\",\"configOptions\":[{\"id\":\"model\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"opencode/big-pickle\",\"options\":[{\"value\":\"opencode/big-pickle\",\"name\":\"Big Pickle\"},{\"value\":\"opencode/smol\",\"name\":\"Smol\"}]},{\"id\":\"mode\",\"name\":\"Mode\",\"category\":\"mode\",\"type\":\"select\",\"currentValue\":\"build\",\"options\":[{\"value\":\"build\",\"name\":\"Build\"},{\"value\":\"plan\",\"name\":\"Plan\"}]}]}}"
else
  exit 1
fi

# ---- turns --------------------------------------------------------------
# The initial prompt, then a fresh session/prompt per turn-boundary steer
# (no `_session/steering` extension). After the turn(s) the script ends, so
# the harness sees stdout EOF and winds the stream down.

CONFIG_SETS=""
read -r promptline || exit 1
while has "$promptline" '"method":"session/set_config_option"'; do
  emit "{\"id\":$(rid "$promptline"),\"result\":{}}"
  CONFIG_SETS="$CONFIG_SETS $promptline"
  read -r promptline || exit 1
done
if ! has "$promptline" '"method":"session/prompt"'; then
  exit 1
fi
pid=$(rid "$promptline")

case "$promptline" in

  *scenario:happy*)
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello from opencode"}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
    ;;

  *scenario:config*)
    # The request carries a model different from the advertised current, so the
    # model set must have arrived. opencode has no thought_level option: NO
    # effort set is legal here.
    if has "$CONFIG_SETS" '"configId":"model"' && has "$CONFIG_SETS" '"value":"opencode/smol"'; then
      update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"configured"}}'
      emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
    else
      emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
    fi
    ;;

*scenario:steer-tb*)
    # First turn settles normally; the steer (no extension) is the NEXT
    # session/prompt (the run requests the current model, so no set arrives).
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
    # Hold the turn open so the steer lands while turn.is_some(): with no
    # steering extension the harness queues it and promotes it here, instead
    # of taking the busy-session cancel path.
    sleep 1
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
    # The turn-boundary steer, promoted to a plain session/prompt.
    read -r followline || exit 1
    fid=$(rid "$followline")
    if has "$followline" '"method":"session/prompt"' && has "$followline" 'redirect please'; then
      update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"boundary"}}'
      emit "{\"id\":$fid,\"result\":{\"stopReason\":\"end_turn\"}}"
    else
      emit "{\"id\":$fid,\"result\":{\"stopReason\":\"refusal\"}}"
    fi
    ;;

  *)
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
    ;;

  esac