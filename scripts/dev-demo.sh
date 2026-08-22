#!/usr/bin/env bash
# One-command demo: boots a seeded engine daemon + the headed app, offline.
# Made for judging look & feel with real input — no edge, no auth needed.
#
#   scripts/dev-demo.sh            # build, seed demo data, open the app
#   scripts/dev-demo.sh --slow     # pace mock streams (~10s) to watch streaming
#
# Everything lives under /tmp/zeron-demo-*; re-runs reuse it. Ctrl-C cleans up.
set -euo pipefail
cd "$(dirname "$0")/.."

DAEMON_DIR=/tmp/zeron-demo-daemon
UI_DIR=/tmp/zeron-demo-ui
IPC=27921
DELAY=""
[[ "${1:-}" == "--slow" ]] && DELAY=350

echo "▸ building (first run takes a few minutes)…"
cargo build -p zeron -q

echo "▸ starting engine daemon on :$IPC"
env ZERON_DATA_DIR="$DAEMON_DIR" ZERON_IPC_PORT=$IPC ZERON_HARNESS=mock \
  ${DELAY:+ZERON_MOCK_DELAY_MS=$DELAY} RUST_LOG=warn \
  ./target/debug/zeron headless &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  (exec 3<>/dev/tcp/127.0.0.1/$IPC) 2>/dev/null && { exec 3>&-; break; }
  sleep 0.25
done

probe() { cargo run -q -p zeron-rpc --example rpc_probe -- "ws://127.0.0.1:$IPC" "$@"; }

if [[ ! -f "$DAEMON_DIR/.demo-seeded" ]]; then
  echo "▸ seeding demo chats"
  DEV=$(probe LocalDevice '{}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["deviceId"])')
  # One space per demo folder, created up-front (chats join by space id).
  # name=sid pairs in a plain string — macOS ships bash 3.2 (no declare -A).
  SPACES_LIST=""
  space_id() { printf '%s\n' "$SPACES_LIST" | tr ' ' '\n' | awk -F= -v n="$1" '$1==n{print $2}'; }
  for project in zeron soccertcg zeron aether; do
    sid=$(uuidgen | tr 'A-Z' 'a-z')
    probe Mutate "{\"op\":\"createSpace\",\"spaceId\":\"$sid\",\"deviceId\":\"$DEV\",\"path\":\"$HOME/github/$project\"}" >/dev/null
    SPACES_LIST="$SPACES_LIST $project=$sid"
  done
  seed() { # title project branch age_hours run
    local id; id=$(uuidgen | tr 'A-Z' 'a-z')
    local sid; sid=$(space_id "$2")
    probe Mutate "{\"op\":\"createChat\",\"chatId\":\"$id\",\"spaceId\":\"$sid\",\"config\":{\"harness\":\"mock\",\"model\":\"fable-5\",\"reasoning\":null,\"sandbox\":\"workspace-write\"}}" >/dev/null
    probe Mutate "{\"op\":\"renameChat\",\"chatId\":\"$id\",\"title\":\"$1\"}" >/dev/null
    probe Mutate "{\"op\":\"setChatBranch\",\"chatId\":\"$id\",\"branch\":\"$3\"}" >/dev/null
    if [[ "$5" == run ]]; then
      probe QueueCommand "{\"chatId\":\"$id\",\"command\":{\"kind\":\"run\",\"messageId\":\"$(uuidgen)\",\"request\":{\"prompt\":\"Walk me through the streaming pipeline\",\"model\":null,\"reasoning\":null,\"modelOptions\":{},\"cwd\":\"/tmp\",\"sandbox\":\"workspace-write\",\"autoApprove\":true,\"resume\":null}}}" >/dev/null
      sleep 1
    fi
    probe Mutate "{\"op\":\"setChatActivity\",\"chatId\":\"$id\",\"lastMessageAt\":$(( ($(date +%s) - $4*3600) * 1000 ))}" >/dev/null
  }
  seed "Native Zeron Rust Rewrite"    zeron zeron/main                 0  run
  seed "Rebalance Player Stats Caps"  soccertcg    zeron/rebalance-player-stat-caps  2  run
  seed "Craft Premium TCG Experience" soccertcg    zeron/craft-premium-tcg-exp       26 skip
  seed "Initial Context Exploration"  zeron        zeron/initial-context-exploration 14 skip
  seed "Soccer TCG Repo Creation"     aether       aether/main                       48 skip
  touch "$DAEMON_DIR/.demo-seeded"
fi

echo "▸ opening zeron (composer is live — type into it; --slow shows streaming)"
ZERON_DATA_DIR="$UI_DIR" ZERON_IPC_PORT=$IPC RUST_LOG=warn ./target/debug/zeron
