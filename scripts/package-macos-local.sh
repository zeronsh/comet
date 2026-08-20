#!/usr/bin/env bash
# Build an isolated local macOS app channel without changing the production
# Zeron.app packager.
#
# Usage:
#   scripts/package-macos-local.sh li [--install]
#   scripts/package-macos-local.sh dev [--install]
#
# --install copies the bundle to ~/Applications. Li deliberately shares the
# official app's data and engine port (alternate app identity, same activity),
# while Dev isolates state and ports for unfinished work.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHANNEL="${1:-}"
if [[ "$CHANNEL" == "-h" || "$CHANNEL" == "--help" ]]; then
  sed -n '2,13p' "$0"
  exit 0
fi
INSTALL=false
shift || true
while (($#)); do
  case "$1" in
    --install) INSTALL=true ;;
    -h|--help)
      sed -n '2,13p' "$0"
      exit 0
      ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

case "$CHANNEL" in
  li)
    APP_NAME="Zeron Li"
    BUNDLE_ID="dev.kalibetre.zeron.li"
    DATA_DIR_NAME=".zeron"
    IPC_PORT=27654
    CALLBACK_PORT=27641
    PROFILE=release
    BADGE_TEXT="LI"
    BADGE_COLOR="8EF3E8"
    TINT_DARK="001B24"
    TINT_LIGHT="39E6D4"
    ;;
  dev)
    APP_NAME="Zeron Dev"
    BUNDLE_ID="dev.kalibetre.zeron.dev"
    DATA_DIR_NAME=".zeron-dev"
    IPC_PORT=27655
    CALLBACK_PORT=27642
    PROFILE=debug
    BADGE_TEXT="DEV"
    BADGE_COLOR="FFD080"
    TINT_DARK="2A0A00"
    TINT_LIGHT="FF9F1C"
    ;;
  *)
    echo "usage: $0 {li|dev} [--install]" >&2
    exit 2
    ;;
esac

command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
TARGET="aarch64-apple-darwin"
rustup target list --installed | grep -qx "$TARGET" || {
  echo "missing Rust target $TARGET; run: rustup target add $TARGET" >&2
  exit 1
}
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/local-apps"
APP="$OUT_DIR/$APP_NAME.app"
INSTALL_DIR="$HOME/Applications"
INSTALLED_APP="$INSTALL_DIR/$APP_NAME.app"
ICON_PNG="$OUT_DIR/${CHANNEL}-icon-1024.png"
ICONSET="$OUT_DIR/${CHANNEL}.iconset"
ICON_FILE="zeron-${CHANNEL}.icns"

cd "$ROOT"
if [[ "$PROFILE" == release ]]; then
  cargo build --release -p zeron --target "$TARGET"
  BIN="$ROOT/target/$TARGET/release/zeron"
else
  cargo build -p zeron --target "$TARGET"
  BIN="$ROOT/target/$TARGET/debug/zeron"
fi
lipo "$BIN" -verify_arch arm64
[[ "$(lipo "$BIN" -archs)" == "arm64" ]] || {
  echo "refusing non-arm64 local app binary: $BIN" >&2
  exit 1
}

rm -rf "$APP" "$ICONSET"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$ICONSET"
# CFBundleExecutable must be the native Mach-O itself. A shell-script primary
# executable makes codesign classify the app as a generic bundle and causes
# macOS to warn about /bin/sh's Intel compatibility slice.
install -m 755 "$BIN" "$APP/Contents/MacOS/zeron"

APP_NAME="$APP_NAME" BUNDLE_ID="$BUNDLE_ID" VERSION="$VERSION" \
ICON_FILE="$ICON_FILE" CHANNEL="$CHANNEL" \
DATA_DIR="$HOME/$DATA_DIR_NAME" IPC_PORT="$IPC_PORT" \
CALLBACK_PORT="$CALLBACK_PORT" \
python3 - "$ROOT/dist/macos/Info.plist" "$APP/Contents/Info.plist" <<'PY'
import os
import plistlib
import sys

with open(sys.argv[1], "rb") as source:
    plist = plistlib.load(source)
plist.update({
    "CFBundleDisplayName": os.environ["APP_NAME"],
    "CFBundleName": os.environ["APP_NAME"],
    "CFBundleIdentifier": os.environ["BUNDLE_ID"],
    "CFBundleExecutable": "zeron",
    "CFBundleIconFile": os.environ["ICON_FILE"],
    "CFBundleShortVersionString": os.environ["VERSION"],
    "CFBundleVersion": os.environ["VERSION"],
    "LSEnvironment": {
        "ZERON_DATA_DIR": os.environ["DATA_DIR"],
        "ZERON_IPC_PORT": os.environ["IPC_PORT"],
        "ZERON_CALLBACK_PORT": os.environ["CALLBACK_PORT"],
        "ZERON_CURSOR_STATE_DIR": os.path.join(os.environ["DATA_DIR"], "cursor-state"),
        "ZERON_WORKTREES_DIR": os.path.join(os.environ["DATA_DIR"], "worktrees"),
        "ZERON_DISABLE_UPDATES": "1",
    },
    "ZeronChannel": os.environ["CHANNEL"],
})
with open(sys.argv[2], "wb") as target:
    plistlib.dump(plist, target, sort_keys=False)
PY

# Generate a recognisable channel badge without committing duplicate 1.3 MB
# source icons. Pillow is only a local packaging dependency.
python3 -c 'import PIL' 2>/dev/null ||
  python3 -m pip install --quiet --user pillow 2>/dev/null ||
  python3 -m pip install --quiet --user --break-system-packages pillow
BADGE_TEXT="$BADGE_TEXT" BADGE_COLOR="$BADGE_COLOR" \
TINT_DARK="$TINT_DARK" TINT_LIGHT="$TINT_LIGHT" \
python3 - "$ROOT/dist/macos/icon-1024.png" "$ICON_PNG" <<'PY'
import os
import sys
from PIL import Image, ImageDraw, ImageFilter, ImageFont, ImageOps

original = Image.open(sys.argv[1]).convert("RGBA")
alpha = original.getchannel("A")
# Re-map the complete artwork through a channel palette. Luminance, texture,
# baked shadow, and transparency survive, but no purple-dominant area remains
# to make Li/Dev look like the official icon at Dock size.
source = ImageOps.colorize(
    ImageOps.grayscale(original),
    black="#" + os.environ["TINT_DARK"],
    white="#" + os.environ["TINT_LIGHT"],
).convert("RGBA")
source.putalpha(alpha)
text = os.environ["BADGE_TEXT"]
color = "#" + os.environ["BADGE_COLOR"]
font_path = "/System/Library/Fonts/SFNS.ttf"
try:
    font = ImageFont.truetype(font_path, 116 if text == "LI" else 92)
except OSError:
    font = ImageFont.load_default()

# Badge sits inside the existing macOS icon's lower-right safe area.
draw_probe = ImageDraw.Draw(source)
bbox = draw_probe.textbbox((0, 0), text, font=font, stroke_width=0)
text_w, text_h = bbox[2] - bbox[0], bbox[3] - bbox[1]
pad_x, pad_y = 70, 45
width, height = text_w + pad_x * 2, text_h + pad_y * 2
right, bottom = 916, 900
box = (right - width, bottom - height, right, bottom)

shadow = Image.new("RGBA", source.size, (0, 0, 0, 0))
shadow_draw = ImageDraw.Draw(shadow)
shadow_box = tuple(v + (10 if i % 2 == 0 else 14) for i, v in enumerate(box))
shadow_draw.rounded_rectangle(shadow_box, radius=58, fill=(0, 0, 0, 190))
shadow = shadow.filter(ImageFilter.GaussianBlur(18))
source.alpha_composite(shadow)

draw = ImageDraw.Draw(source)
draw.rounded_rectangle(box, radius=58, fill=color, outline=(255, 255, 255, 235), width=10)
text_x = box[0] + (width - text_w) / 2
text_y = box[1] + (height - text_h) / 2 - bbox[1]
draw.text((text_x, text_y), text, font=font, fill=(7, 14, 25, 255))
source.save(sys.argv[2], optimize=True)
PY

for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_PNG" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  retina=$((size * 2))
  sips -z "$retina" "$retina" "$ICON_PNG" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/$ICON_FILE"
rm -rf "$ICONSET"

# Ad-hoc signing is sufficient for a bundle built locally on this Mac.
codesign --deep --force --sign - "$APP" >/dev/null

echo "built: $APP"
if $INSTALL; then
  mkdir -p "$INSTALL_DIR"
  rm -rf "$INSTALLED_APP"
  ditto "$APP" "$INSTALLED_APP"
  # Register immediately so Spotlight, Dock, and notification attribution see
  # the unique bundle id/icon without waiting for a filesystem scan.
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "$INSTALLED_APP" >/dev/null 2>&1 || true
  echo "installed: $INSTALLED_APP"
  echo "launch: open '$INSTALLED_APP'"
fi
