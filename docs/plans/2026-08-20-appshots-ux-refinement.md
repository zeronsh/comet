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
