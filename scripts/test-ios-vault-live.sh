#!/usr/bin/env bash
# Actual iOS clients <-> local workerd <-> Rust vault/HostRelay.
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"
run_dir=/tmp/comet-mobile-e2e
mkdir -p "$run_dir"
if ! mkdir "$run_dir/lock" 2>/dev/null; then
  echo "Another mobile vault test owns $run_dir/lock" >&2
  exit 1
fi
host_pid=
edge_pid=
cleanup() {
  if [[ -n "$host_pid" ]]; then kill "$host_pid" 2>/dev/null || true; fi
  if [[ -n "$edge_pid" ]]; then kill "$edge_pid" 2>/dev/null || true; fi
  rm -f "$run_dir/connection.json" "$run_dir/revoke" "$run_dir/revoked" "$run_dir/done"
  rmdir "$run_dir/lock"
}
trap cleanup EXIT
rm -f "$run_dir/connection.json" "$run_dir/revoke" "$run_dir/revoked" "$run_dir/done"
port="${ZERON_MOBILE_TEST_PORT:-27641}"
edge_url="http://127.0.0.1:$port"
(cd edge && exec ./node_modules/.bin/wrangler dev --local --ip 127.0.0.1 --port "$port" \
  --inspector-port 0 --var AUTH_MODE:dev --persist-to "$run_dir/edge") > "$run_dir/edge.log" 2>&1 &
edge_pid=$!
for _ in {1..100}; do
  if curl -s -o /dev/null "$edge_url"; then break; fi
  if ! kill -0 "$edge_pid" 2>/dev/null; then cat "$run_dir/edge.log"; exit 1; fi
  sleep 0.2
done
ZERON_VAULT_EDGE_URL="$edge_url" ZERON_MOBILE_E2E_DIR="$run_dir" \
  cargo test -p zeron-engine --test vault_e2e mobile_test_host -- --nocapture > "$run_dir/host.log" 2>&1 &
host_pid=$!
for _ in {1..600}; do
  if [[ -f "$run_dir/connection.json" ]]; then break; fi
  if ! kill -0 "$host_pid" 2>/dev/null; then cat "$run_dir/host.log"; exit 1; fi
  sleep 0.2
done
[[ -f "$run_dir/connection.json" ]] || { echo "Host did not start; see $run_dir/host.log" >&2; exit 1; }
xcodebuild -project "$root/apps/ios/Zeron.xcodeproj" -scheme Zeron \
  -destination "${ZERON_IOS_TEST_DESTINATION:-platform=iOS Simulator,name=iPhone 17 Pro}" \
  -derivedDataPath "${ZERON_IOS_TEST_BUILD_DIR:-/tmp/comet-ios-e2ee}" \
  -only-testing:ZeronTests/MobileVaultLiveTests \
  -only-testing:ZeronTests/VaultChannelTests \
  -only-testing:ZeronTests/SessionSidecarsTests test > "$run_dir/ios.log" 2>&1 || {
    tail -80 "$run_dir/ios.log"
    exit 1
  }
wait "$host_pid"
host_pid=
echo "iOS enrollment, encrypted RPC, sidecars, reconnect and revocation passed. Logs: $run_dir"
