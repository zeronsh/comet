# Appshots UX refinement implementation plan

1. Extend `crates/ui/src/appshots.rs` and `appshots/macos.rs` with optional
   application-icon presentation data and separate Screen Recording and
   Accessibility permission requests.
2. Replace the vertical Appshot rows in `crates/ui/src/composer.rs` with a
   fixed-height horizontal visual tray modeled on the proven Codex treatment;
   preserve preview, removal, upload, routing, and prompt serialization.
3. Rework `crates/ui/src/settings/shortcuts.rs` into an explicit permission
   checklist. Do not request permissions merely because the feature toggle was
   enabled, and label Accessibility as optional.
4. Update pure layout tests and permission-facing copy. Validate with
   `cargo test -p zeron-ui --lib`, `cargo check -p zeron-ui`,
   `cargo build -p zeron`, and `git diff --check`.

## Composer visual normalization follow-up

1. Record the PNG pixel dimensions in `crates/ui/src/appshots.rs` and
   `appshots/macos.rs`, then compute an explicit contained display size for a
   shared 208×132 image box in `crates/ui/src/composer.rs`.
2. Make both the visual stage and the complete Appshot tile clipping
   boundaries, bottom-align every aspect ratio, and apply a native bottom
   edge-fade to the screenshot before layering the application icon.
3. Increase the icon-to-label spacing while keeping one fixed-height,
   horizontally scrolling tray. Capture, removal, preview, persistence,
   transport, and permission controls remain unchanged.
4. Add dimension-parser and landscape/portrait/square sizing tests, run the
   `zeron-ui` test suite and macOS build checks, then inspect the result in the
   signed `Zeron Dev.app`.
