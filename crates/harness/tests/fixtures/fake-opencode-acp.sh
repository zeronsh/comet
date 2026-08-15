#!/bin/sh
# Fake OpenCode ACP agent for model-dependent config sequencing tests.
#
# The initial effort ladder belongs to openai/model-a. Selecting
# anthropic/model-b returns a fresh configOptions snapshot with a different
# effort ladder. The fixture rejects an old effort, an unnecessary mode write,
# or a second model write.

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

update() {
  emit "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"$SID\",\"update\":$1}}"
}

read -r line || exit 1
has "$line" '"method":"initialize"' || exit 1
has "$line" '"protocolVersion":1' || exit 1
has "$line" '"name":"zeron"' || exit 1
emit "{\"id\":$(rid "$line"),\"result\":{\"protocolVersion\":1}}"

read -r line || exit 1
has "$line" '"method":"session/new"' || exit 1
has "$line" '"mcpServers":[]' || exit 1
SID="opencode-session"
emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"$SID\",\"configOptions\":[{\"id\":\"model\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"openai/model-a\",\"options\":[{\"value\":\"openai/model-a\",\"name\":\"OpenAI Model A\"},{\"value\":\"anthropic/model-b\",\"name\":\"Anthropic Model B\"}]},{\"id\":\"effort\",\"name\":\"Effort\",\"category\":\"thought_level\",\"type\":\"select\",\"currentValue\":\"medium\",\"options\":[{\"value\":\"low\",\"name\":\"Low\"},{\"value\":\"medium\",\"name\":\"Medium\"}]},{\"id\":\"mode\",\"name\":\"Session Mode\",\"category\":\"mode\",\"type\":\"select\",\"currentValue\":\"build\",\"options\":[{\"value\":\"build\",\"name\":\"Build\"},{\"value\":\"plan\",\"name\":\"Plan\"}]}]}}"

read -r line || exit 0
valid=1
model_set=0
effort_set=0
mode_set=0
state="initial"

while has "$line" '"method":"session/set_config_option"'; do
  id=$(rid "$line")
  if [ "$state" = "initial" ]; then
    if has "$line" '"configId":"model"' && has "$line" '"value":"anthropic/model-b"'; then
      model_set=1
      state="selected"
      # OpenCode's model response is the refreshed snapshot used by the next
      # config phase.
      emit "{\"id\":$id,\"result\":{\"configOptions\":[{\"id\":\"model\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"anthropic/model-b\",\"options\":[{\"value\":\"openai/model-a\",\"name\":\"OpenAI Model A\"},{\"value\":\"anthropic/model-b\",\"name\":\"Anthropic Model B\"}]},{\"id\":\"effort\",\"name\":\"Effort\",\"category\":\"thought_level\",\"type\":\"select\",\"currentValue\":\"high\",\"options\":[{\"value\":\"high\",\"name\":\"High\"},{\"value\":\"xhigh\",\"name\":\"XHigh\"}]},{\"id\":\"mode\",\"name\":\"Session Mode\",\"category\":\"mode\",\"type\":\"select\",\"currentValue\":\"build\",\"options\":[{\"value\":\"build\",\"name\":\"Build\"},{\"value\":\"plan\",\"name\":\"Plan\"}]}]}}"
    else
      valid=0
      emit "{\"id\":$id,\"result\":{}}"
    fi
  else
    if has "$line" '"configId":"effort"' && has "$line" '"value":"xhigh"'; then
      effort_set=1
    elif has "$line" '"configId":"mode"'; then
      mode_set=1
      valid=0
    elif has "$line" '"configId":"model"'; then
      valid=0
    else
      valid=0
    fi
    emit "{\"id\":$id,\"result\":{}}"
  fi
  read -r line || exit 1
done

has "$line" '"method":"session/prompt"' || exit 1
pid=$(rid "$line")
if [ "$valid" -eq 1 ] && [ "$model_set" -eq 1 ] && [ "$effort_set" -eq 1 ] && [ "$mode_set" -eq 0 ]; then
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"configured"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
else
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
fi
