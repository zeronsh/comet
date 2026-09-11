# Appshots screenshot evidence

These 20 frames show the Appshots implementation using neutral fixtures: 14 native GPUI Metal exports and 6 iPhone 17 Pro simulator screenshots. The desktop source was unchanged by the v0.2.60 rebase. Light desktop exports omit the macOS compositor backdrop and therefore appear gray.

Desktop coverage includes light/dark, narrow layouts, composer and transcript cards, queue previews, uploading/unavailable states, dedicated settings, keyboard enablement, destination selection and shortcut recording. Phone coverage includes portrait/landscape, horizontal cards, full-image preview, queue gallery and actions.

These are presentation fixtures, not proof of live application capture, permission dialogs, portal interaction or physical remote-device delivery. The iOS offline host disables Send now. The desktop fixture emitted nonfatal resize and local IPC probe warnings while exporting all frames.

Reproduce desktop frames with the `appshots-fixture` example and supplied neutral PNG inputs. Reproduce phone frames with `AppshotUITests` using the `-demo -appshots` dataset. See `docs/appshots.md` for the implementation and validation limits.
