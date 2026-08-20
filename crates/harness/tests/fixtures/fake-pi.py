#!/usr/bin/env python3
import json
import sys

scenario = "happy"
model_set = False
thinking_set = False
session_file = "/tmp/fake-pi-session.jsonl"
if "--session" in sys.argv:
    session_file = sys.argv[sys.argv.index("--session") + 1]


def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def response(command, request_id=None, success=True, data=None, error=None):
    value = {"type": "response", "command": command, "success": success}
    if request_id is not None:
        value["id"] = request_id
    if data is not None:
        value["data"] = data
    if error is not None:
        value["error"] = error
    send(value)


# Real Pi extensions may emit UI notifications before the first command response.
send({"type": "extension_ui_request", "id": "startup-status", "method": "setStatus", "statusKey": "fake", "statusText": "ready"})

for raw in sys.stdin:
    try:
        command = json.loads(raw)
    except json.JSONDecodeError:
        response("parse", success=False, error="invalid json")
        continue

    kind = command.get("type")
    request_id = command.get("id")

    if kind == "switch_session":
        # Session replacement is unsafe for extensions with delayed callbacks;
        # the native harness must pass --session when spawning instead.
        sys.stderr.write("switch_session must not be used\n")
        sys.stderr.flush()
        sys.exit(91)
    elif kind == "get_state":
        response(kind, request_id, data={
            "model": {"provider": "openai-codex", "id": "gpt-5.6-sol", "name": "GPT-5.6 Sol"},
            "thinkingLevel": "medium",
            "isStreaming": False,
            "sessionFile": session_file,
            "sessionId": "fake-pi-session",
        })
    elif kind == "get_available_models":
        response(kind, request_id, data={"models": [
            {
                "provider": "openai-codex",
                "id": "gpt-5.6-sol",
                "name": "GPT-5.6 Sol",
                "reasoning": True,
                "input": ["text", "image"],
                "thinkingLevelMap": {"minimal": "low", "xhigh": "xhigh", "max": "xhigh"},
            },
            {
                "provider": "anthropic",
                "id": "claude-test-20260101",
                "name": "Pinned Claude",
                "reasoning": False,
                "input": ["text"],
            },
        ]})
    elif kind == "get_commands":
        response(kind, request_id, data={"commands": [
            {"name": "fix-tests", "description": "Fix the tests", "source": "prompt"},
            {"name": "skill:review", "description": "Review changes", "source": "skill"},
        ]})
    elif kind == "set_model":
        if command.get("modelId") == "reject":
            response(kind, request_id, success=False, error="Model not found: reject")
        else:
            model_set = True
            response(kind, request_id, data={"provider": command.get("provider"), "id": command.get("modelId")})
    elif kind == "set_thinking_level":
        thinking_set = command.get("level") == "high"
        response(kind, request_id)
    elif kind == "prompt":
        scenario = command.get("message", "happy")
        # Exercise response/event multiplexing: an event is legal before the
        # command's acceptance response reaches the client.
        send({"type": "agent_start"})
        response(kind, request_id)

        if scenario == "happy":
            if not (model_set and thinking_set):
                send({"type": "extension_error", "extensionPath": "fake", "event": "prompt", "error": "model/thinking setup missing"})
            send({"type": "message_start", "message": {"role": "assistant", "content": []}})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "Hello"}})
            send({"type": "message_update", "assistantMessageEvent": {"type": "toolcall_end", "contentIndex": 1, "toolCall": {"id": "call-1", "name": "bash", "arguments": {"command": "printf hi"}}}})
            send({"type": "tool_execution_start", "toolCallId": "call-1", "toolName": "bash", "args": {"command": "printf hi"}})
            send({"type": "tool_execution_end", "toolCallId": "call-1", "toolName": "bash", "result": {"content": [{"type": "text", "text": "hi"}]}, "isError": False})
            send({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "Hello"}], "usage": {"input": 12, "output": 3}, "stopReason": "stop"}})
            send({"type": "agent_end", "messages": [], "willRetry": False})
            send({"type": "message_start", "message": {"role": "assistant", "content": []}})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": " after-agent-end"}})
            send({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": " after-agent-end"}], "stopReason": "stop"}})
            send({"type": "extension_ui_request", "id": "confirm-1", "method": "confirm", "title": "Continue?", "message": "Allow the final step?"})
        elif scenario == "fallback":
            send({"type": "message_start", "message": {"role": "assistant", "content": []}})
            send({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "non-streamed"}], "stopReason": "stop"}})
            send({"type": "agent_settled"})
        elif scenario == "steer":
            send({"type": "message_start", "message": {"role": "assistant", "content": []}})
            send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "before"}})
        elif scenario == "interrupt":
            pass
    elif kind == "extension_ui_response":
        if command.get("id") == "confirm-1" and command.get("confirmed") is True:
            send({"type": "agent_settled"})
        else:
            send({"type": "extension_error", "extensionPath": "fake", "event": "confirm", "error": "dialog response incorrect"})
            send({"type": "agent_settled"})
    elif kind == "steer":
        response(kind, request_id)
        send({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "before"}], "stopReason": "stop"}})
        send({"type": "message_start", "message": {"role": "assistant", "content": []}})
        send({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "+steered:" + command.get("message", "")}})
        send({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "+steered:" + command.get("message", "")}], "stopReason": "stop"}})
        send({"type": "agent_settled"})
    elif kind == "abort":
        response(kind, request_id)
        send({"type": "agent_settled"})
    else:
        response(kind or "unknown", request_id, success=False, error="unsupported command")
