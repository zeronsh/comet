# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/zeron-<version>-linux-<arch>.tar.gz` containing:

- `zeron` — the binary (headed by default; `zeron headless` runs the engine alone)
- `zeron.desktop` — XDG desktop entry
- `zeron.png` — 1024×1024 Zeron app icon
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/zeron-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Zeron.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
dmg. The auto-update tarball retains an internal `Zeron.app` path so older
installed builds can update into Zeron. CI runs this on tags
(`.github/workflows/release.yml`).

### Local Li and Dev channels

Personal builds can coexist with the official application under distinct
bundle identities and icons. Li is an alternate daily frontend for the same
activity; Dev is the isolated unfinished-work channel:

```sh
scripts/package-macos-local.sh li --install   # ~/Applications/Zeron Li.app
scripts/package-macos-local.sh dev --install  # ~/Applications/Zeron Dev.app
```

| Channel | Bundle ID | Data directory | IPC port | Build |
| --- | --- | --- | --- | --- |
| Official | `sh.zeron.app` | `~/.zeron` | `27654` | release |
| Li | `dev.kalibetre.zeron.li` | `~/.zeron` | `27654` | release |
| Dev | `dev.kalibetre.zeron.dev` | `~/.zeron-dev` | `27655` | debug |

The local packager explicitly builds and verifies native
`aarch64-apple-darwin` Mach-O executables. Channel variables live in each
bundle's `LSEnvironment`, keeping `CFBundleExecutable` native rather than a
shell launcher. The bundles use tinted/badged icons and set
`ZERON_DISABLE_UPDATES=1`: the public updater must never replace a personal
build. Official and Li share one
engine and activity store. Whichever starts first owns that engine; the other
app connects to it. Quit Li before launching Official when comparing the
actual official engine version rather than only its UI. Dev is fully isolated,
including its callback port, worktrees, and Cursor state.

The manual production steps follow, for reference (run on a macOS host — gpui
needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p zeron --target aarch64-apple-darwin
   cargo build --release -p zeron --target x86_64-apple-darwin
   lipo -create -output zeron \
     target/aarch64-apple-darwin/release/zeron \
     target/x86_64-apple-darwin/release/zeron
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Zeron.app/Contents/{MacOS,Resources}
   cp zeron Zeron.app/Contents/MacOS/zeron
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Zeron.app/Contents/Info.plist
   ```
3. Icon: generate `zeron.icns` from `dist/macos/icon-1024.png` (the macOS-shaped
   variant of the artwork — squircle mask, margins, and shadow pre-baked, since
   `sips` can't apply an alpha mask) and place it at
   `Zeron.app/Contents/Resources/zeron.icns`:
   ```sh
   mkdir zeron.iconset && sips -z 256 256 dist/macos/icon-1024.png --out zeron.iconset/icon_256x256.png
   iconutil -c icns zeron.iconset -o Zeron.app/Contents/Resources/zeron.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Zeron.app
   xcrun notarytool submit Zeron.zip --keychain-profile … --wait
   xcrun stapler staple Zeron.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Zeron -srcfolder Zeron.app -ov -format UDZO Zeron.dmg`).
