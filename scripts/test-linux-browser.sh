#!/usr/bin/env bash
# Run the real shell browser fixture with native mouse input on X11 and Wayland.
# Captures stay outside the repository and can be uploaded as PR attachments.
set -euo pipefail
output="${1:?usage: test-linux-browser.sh OUTPUT_DIRECTORY}"
binary="${BROWSER_FIXTURE_BINARY:-target/release/examples/browser-fixture}"
mkdir -p "$output"
output="$(realpath "$output")"
runtime="$(mktemp -d)"
chmod 700 "$runtime"
processes=()
cleanup() {
  for pid in "${processes[@]}"; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$runtime"
}
trap cleanup EXIT
Xvfb -displayfd 3 -screen 0 1280x800x24 -ac -nolisten tcp 3>"$runtime/display" >"$output/xvfb.log" 2>&1 &
xvfb_pid=$!
processes+=("$xvfb_pid")
display_number=
for _ in $(seq 1 300); do
  if IFS= read -r display_number <"$runtime/display"; then break; fi
  if ! kill -0 "$xvfb_pid" 2>/dev/null; then
    echo "Xvfb exited before publishing a display" >&2
    cat "$output/xvfb.log" >&2
    exit 1
  fi
  sleep .1
done
if [[ ! "$display_number" =~ ^[0-9]+$ ]]; then
  echo "Xvfb did not publish a valid display within 30 seconds" >&2
  cat "$output/xvfb.log" >&2
  exit 1
fi
export DISPLAY=":$display_number"
export XDG_RUNTIME_DIR="$runtime"
export ZERON_BROWSER_NATIVE_POINTER=1
openbox >"$output/openbox.log" 2>&1 &
processes+=("$!")
record_fixture() {
  local mode="$1" capture_window="${2:-}" fixture_pid video_pid
  mkdir -p "$output/$mode"
  "$binary" "$output/$mode" >"$output/$mode/fixture.log" 2>&1 &
  fixture_pid=$!
  processes+=("$fixture_pid")
  if [ -z "$capture_window" ]; then
    for _ in $(seq 1 100); do
      capture_window="$(xdotool search --onlyvisible --pid "$fixture_pid" 2>/dev/null | head -1 || true)"
      [ -n "$capture_window" ] && break
      kill -0 "$fixture_pid" 2>/dev/null || { cat "$output/$mode/fixture.log"; return 1; }
      sleep .1
    done
  fi
  [ -n "$capture_window" ]
  ffmpeg -nostdin -loglevel error -y -f x11grab -framerate 30 -window_id "$capture_window" -i "$DISPLAY" \
    -c:v libx264 -preset veryfast -crf 20 -pix_fmt yuv420p -movflags +faststart \
    "$output/$mode/browser.mp4" >"$output/$mode/video.log" 2>&1 &
  video_pid=$!
  processes+=("$video_pid")
  local result=0
  wait "$fixture_pid" || result=$?
  kill -INT "$video_pid" 2>/dev/null || true
  wait "$video_pid" || true # X11 grab may end when the fixture closes its window.
  cat "$output/$mode/fixture.log"
  [ "$result" -eq 0 ]
  test -f "$output/$mode/result.txt"
  test -f "$output/$mode/linux-results.json"
  ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of json "$output/$mode/browser.mp4"
}
export WAYLAND_DISPLAY= GDK_BACKEND=x11
record_fixture x11
export WAYLAND_DISPLAY=zeron-browser-test GDK_BACKEND=wayland
weston --backend=x11 --renderer=pixman --shell=kiosk-shell.so --socket="$WAYLAND_DISPLAY" \
  --idle-time=0 --width=1000 --height=680 >"$output/weston.log" 2>&1 &
processes+=("$!")
for _ in $(seq 1 100); do
  [ -S "$runtime/$WAYLAND_DISPLAY" ] && break
  sleep .1
done
export ZERON_BROWSER_CAPTURE_WINDOW="$(xdotool search --class weston | head -1)"
record_fixture wayland "$ZERON_BROWSER_CAPTURE_WINDOW"
