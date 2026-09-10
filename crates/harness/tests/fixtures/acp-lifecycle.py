#!/usr/bin/env python3
"""Stateful ACP peer: reject overlapping prompts and expose real error.data."""
import json
import select
import sys
import time


def emit(value):
    print(json.dumps({"jsonrpc": "2.0", **value}), flush=True)


def update(value):
    emit({"method": "session/update", "params": {"sessionId": "s-1", "update": value}})


def reply(request, result):
    emit({"id": request["id"], "result": result})


def error(request, data):
    emit({"id": request["id"], "error": {"code": -32600, "message": "Invalid request", "data": data}})


# Unbuffered reads keep select honest, including multiple queued frames.
buffer = b""


def read(timeout=None):
    global buffer
    deadline = None if timeout is None else time.monotonic() + timeout
    while b"\n" not in buffer:
        left = None if deadline is None else max(0, deadline - time.monotonic())
        if not select.select([sys.stdin], [], [], left)[0]:
            return None
        import os
        data = os.read(sys.stdin.fileno(), 65536)
        if not data:
            sys.exit(0)
        buffer += data
    line, buffer = buffer.split(b"\n", 1)
    return json.loads(line)


turn = 0
while True:
    request = read()
    method = request.get("method")
    if method == "initialize":
        reply(request, {"protocolVersion": 1, "agentCapabilities": {}})
    elif method == "session/new":
        reply(request, {"sessionId": "s-1"})
    elif method == "session/prompt":
        turn += 1
        scenario = request["params"]["prompt"][0]["text"]
        if scenario == "error":
            error(request, {"reason": "A prompt is already running", "retryable": False})
            continue
        if turn > 1:
            update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": scenario}})
            reply(request, {"stopReason": "end_turn"})
            if scenario == "self-continue":
                time.sleep(1.3)
                update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "autonomous"}})
            continue
        if scenario != "reasoning":
            update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "working"}})
        if scenario in ("tools", "open-tool", "cancel", "wedge", "eof"):
            for i in range(4):
                update({"sessionUpdate": "tool_call", "toolCallId": str(i), "kind": "read", "title": "read source", "status": "pending", "rawInput": {"path": "/tmp/source.rs"}})
                if scenario != "open-tool":
                    update({"sessionUpdate": "tool_call_update", "toolCallId": str(i), "status": "completed", "content": []})
        if scenario == "usage":
            update({"sessionUpdate": "usage_update", "used": 100, "size": 10000, "cost": {"amount": 0.01, "currency": "USD"}})
        if scenario == "reasoning":
            update({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "thinking"}})
        # Three retired quiet windows. Any ordinary prompt during this gap
        # must fail, just like a real ACP agent that is awaiting its model.
        deadline = time.monotonic() + 3.6
        cancelled = False
        while time.monotonic() < deadline or scenario in ("cancel", "wedge"):
            incoming = read(max(0, deadline - time.monotonic()) if scenario not in ("cancel", "wedge") else None)
            if incoming is None:
                break
            if incoming.get("method") == "session/cancel":
                if scenario == "wedge":
                    continue
                cancelled = True
                break
            if "id" in incoming:
                error(incoming, {"reason": "A prompt is already running"})
        if scenario == "eof":
            sys.exit(0)
        if not cancelled:
            update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "finished"}})
        reply(request, {"stopReason": "cancelled" if cancelled else "end_turn"})
    elif "id" in request:
        reply(request, {})
