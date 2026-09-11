# Appshots for Zeron

> Historical design notes. The implemented behavior and current validation are
> documented in [Appshots](../appshots.md); proposals below may be superseded.

Status: implemented on the exploration branch; native validation ongoing

Branch: `wip/appshots-exploration`

Scope: desktop Zeron on macOS, Linux X11, and Linux Wayland

## Summary

An Appshot is a user-triggered capture of the frontmost application window. A
global keyboard shortcut works while Zeron is in the background, captures both
the visible window and machine-readable application context, then stages that
capture in a Zeron composer for review. It is not an attachment-menu action and
does not send a message automatically.

The first useful release should support this vertical slice:

1. The user invokes a global shortcut from any macOS application.
2. Zeron captures the frontmost window before activating itself.
3. Zeron opens the chosen composer and stages a screenshot plus accessibility
   context.
4. The user can inspect, remove, annotate, and send the capture.
5. For a remotely hosted session, the existing queued-attachment path moves the
   screenshot to the host while the semantic context rides in the prompt.

## Reference behavior

Inspection of Codex's installed desktop client shows that its Appshot feature:

- registers a global shortcut (both Command keys on macOS by default);
- selects the frontmost application window without an app picker;
- captures the screenshot, application name, bundle identifier, window title,
  and accessibility-derived text/tree, including off-screen content;
- stages the result rather than immediately sending it;
- supports automatic, last-chat, and new-chat destinations;
- serializes application metadata and accessibility content as structured
  context while also attaching the image;
- requires Screen Recording and Accessibility permission on macOS.

Codex's composer presents each capture as a 232×140 visual tile with the source
application icon over the screenshot, a concise title, hover removal, and one
horizontal attachment tray. Its native helper presents Screen Recording and
Accessibility as separate rows in one guided permission surface.

## Product contract

### Goal

Make it effortless to give an agent rich context about the application the
user is currently looking at, without making the user save a screenshot, switch
to Zeron, attach a file, and manually copy otherwise invisible text.

### Non-goals for the first release

- Capturing an arbitrary rectangular screen region.
- Recording video or continuous screen state.
- Automatically submitting a captured window to an agent.
- Capturing from a headless or remote agent host.
- OCR as the primary source of semantic context.
- Continuous recording or bypassing platform capture consent.
- Replacing or changing the existing paperclip, paste, or drag-and-drop flows.

### User-visible behavior

- The shortcut operates while another application is focused.
- The frontmost eligible window at shortcut time is the capture target.
- Zeron never steals focus until the screenshot has been acquired.
- A staged Appshot is visually distinct from an ordinary image attachment.
- The staged tile shows a large contained screenshot, the source application
  icon, and the window title when available.
- The user can preview the screenshot, remove the Appshot, and type an
  instruction before sending. Semantic text remains attached without adding
  technical character counts to the composer.
- A successful capture never sends on its own.
- A failed or permission-blocked capture presents a concrete recovery action.

### Destination policy

Three settings are proposed:

- **Automatic**: use the visible/recent composer if it can accept input;
  otherwise open a new-session draft.
- **Last session**: stage into the most recently active session composer,
  restoring the window if necessary.
- **New session**: open the new-session canvas with the Appshot staged. The
  existing space/device defaults remain authoritative.

The recommended default is **Automatic**, with a conservative eligibility
rule: only reuse an existing composer if Zeron had an active session selected
recently and that session is still writable. Otherwise create a draft. The
exact recency threshold should be validated in use rather than copied blindly
from another product.

## Control section inventory

### Capture initiation

- Enable or disable Appshots.
- Configure the global shortcut.
- Detect shortcut conflicts and registration failure.
- Show the currently registered shortcut in Settings.

### Capture permissions

- Explain why Screen Recording is needed.
- Explain that Accessibility is optional and adds off-screen application text.
- Never launch both permission prompts from the feature toggle.
- Request each permission from its own explicit user action.
- Open the relevant macOS Settings pane.
- Re-check permission state after the user returns.
- Allow screenshot-only capture whenever Screen Recording is granted; the
  settings checklist, rather than every staged tile, communicates whether
  semantic context is available.

### Destination and draft

- Choose Automatic, Last session, or New session.
- Restore or focus the Zeron window after capture.
- Stage the Appshot without submitting it.
- Preserve the draft if destination resolution or host connectivity is delayed.

### Staged Appshot

- Show source app icon/name and window title.
- Preview the screenshot.
- Show whether application text was captured and offer a disclosure view.
- Remove the Appshot.
- Send it with typed text or as an Appshot-only message.

### Lifecycle and privacy

- Explain which data will be sent.
- Delete abandoned temporary captures.
- Retain sent screenshots under the existing profile-scoped attachment rules.
- Never write accessibility text to logs.
- After successful staging, play a soft confirmation cue. Honor the
  dedicated capture sound setting and `ZERON_DISABLE_SOUND`; failed captures stay silent.

## Architecture

### Placement

Capture belongs to the headed UI side of Zeron, not the engine:

```text
macOS global event / hotkey
          |
          v
native Appshot capture service
          |
          v
GPUI application coordinator -----> composer draft
                                          |
                                          v
                            existing attachment upload
                                          |
                     local or remote session host
```

This boundary is required because the captured desktop belongs to the viewport
machine. A remote engine may be headless, may run on another operating system,
and must not receive local desktop permissions.

### Native service

Define a small platform-neutral Rust interface in the UI crate:

```rust
trait AppshotCaptureService {
    fn register_shortcut(&self, shortcut: GlobalShortcut) -> Result<()>;
    fn permission_state(&self) -> AppshotPermissionState;
    async fn capture_frontmost_window(&self) -> Result<CapturedAppshot>;
}
```

The implementation uses platform backends with independent capability states:

```text
appshots/
├── macos.rs
└── linux/
    ├── mod.rs
    ├── portal.rs
    ├── x11.rs
    └── atspi.rs
```

Linux chooses its shortcut backend at runtime: Wayland uses XDG
Desktop Portal and X11 uses a passive key grab. Capture prefers the portal's
Active Window target wherever it is advertised, then uses the portal picker on
Wayland or EWMH plus direct drawable capture on X11. AT-SPI enrichment is
optional and independent on both Linux paths. Capability reporting
distinguishes ready, permission required, setup required, user selection,
checking, and unavailable.

The macOS capture module is responsible for:

- global shortcut detection;
- frontmost process and window identification;
- ScreenCaptureKit window capture;
- `AXUIElement` traversal and bounded serialization;
- permission probing and Settings deep links;
- returning capture results over a narrow IPC protocol.

The implementation uses an in-process Rust/Objective-C bridge. TCC attribution
is kept correct in development by launching a separately signed `Zeron Dev.app`
through LaunchServices; the bundle also embeds its isolated data directory and
IPC port so privacy-driven relaunches cannot enter the personal instance.

### Capture ordering

Ordering is correctness-sensitive:

1. Observe and identify the current frontmost window.
2. Capture its screenshot and accessibility snapshot.
3. Emit a completed capture to Zeron.
4. Resolve the destination.
5. activate/reveal Zeron and stage the result.

Activating Zeron before steps 1-2 would capture Zeron itself.

### Data model

The composer needs a first-class object rather than treating the capture as an
undifferentiated image:

```rust
struct CapturedAppshot {
    id: String,
    app_name: String,
    bundle_identifier: Option<String>,
    window_title: Option<String>,
    accessibility: AccessibilitySnapshot,
    screenshot: StagedAttachment,
    captured_at: DateTime<Utc>,
}

struct AccessibilitySnapshot {
    format_version: u32,
    content: String,
    truncated: bool,
}
```

`StagedAttachment` remains the screenshot carrier so decoding, thumbnails,
upload progress, queued transfers, transcript caching, and remote delivery are
reused. The enclosing Appshot supplies source identity and semantic context.

Draft state should own Appshots next to ordinary staged attachments. This lets
the UI remove or inspect one coherently and prevents semantic context from
surviving after its screenshot is removed.

### Prompt representation

At send time, serialize Appshots as observed context and append ordinary image
paths through the existing attachment mechanism:

```xml
<appshot app="Safari"
         bundle-identifier="com.apple.Safari"
         window-title="API documentation"
         image="/resolved/path/Safari Appshot.png">
  ...escaped, bounded accessibility snapshot...
</appshot>
```

The prompt should explicitly tell the harness that this is untrusted content
observed in an application, not an instruction from the user. XML is only a
candidate wire representation; the important properties are escaping, version
stability, clear provenance, and an image/context association.

For a first implementation the semantic block can be synthesized into
`RunRequest.prompt`, while `RunRequest.attachments` continues to carry the
screenshot path. That preserves compatibility with older engines and reuses
the existing pending-path rewriting. A dedicated protocol field becomes
worthwhile only if multiple consumers need structured Appshots before harness
dispatch or if transcript rendering must avoid parsing the prompt.

### Remote delivery

The viewer creates the capture bytes. On send:

1. The composer allocates the usual upload identifier.
2. The screenshot follows queued attachment transfer to the chat's host.
3. Pending screenshot paths in both the image list and semantic block are
   resolved by the host.
4. The harness receives an image content block plus the observed application
   context.

No Appshot-specific binary transport is required for the first version.

## Security and privacy

Accessibility content is high-risk input. It may contain secrets, invisible
controls, or prompt-injection text supplied by a website. The implementation
must:

- label it as untrusted observed data;
- escape delimiters and reject malformed metadata;
- cap nodes, depth, per-node text, and total serialized bytes;
- prefer roles, labels, values, and useful document text over geometry noise;
- exclude secure text fields and password values;
- avoid logging payload content;
- keep the capture staged for user review before transmission;
- show degraded state when accessibility extraction is unavailable;
- apply existing profile and attachment isolation to the screenshot.

The disclosure UI should summarize the amount and source of captured text. A
raw-tree inspection view is useful for trust and debugging but can follow the
first working slice.

## Failure behavior

| Failure | User outcome |
| --- | --- |
| Shortcut registration conflict | Setting shows conflict and capture remains disabled. |
| No eligible frontmost window | Non-blocking notice; no draft is changed. |
| Screen Recording denied | Permission explanation and direct recovery action. |
| Accessibility denied | Screenshot-only Appshot with a visible warning. |
| Accessibility traversal times out | Stage the screenshot with partial/truncated context. |
| Zeron window cannot be restored | Preserve capture in an inbox-like pending slot and notify. |
| Remote host is offline | Keep the draft; existing queued-transfer behavior applies on send. |
| Helper crashes | Restart lazily and report capture failure without affecting the engine. |

## Persistence

Settings are device-local because the shortcut and permissions describe a
specific desktop:

- enabled;
- shortcut;
- destination policy;
- optional sound;
- first-use explanation completed.

Unsent captures should initially live only in draft state and temporary files.
If Zeron already persists composer drafts, Appshot metadata and the screenshot
temporary-file contract must be persisted atomically; otherwise the first
slice should explicitly document that an application restart discards unsent
Appshots.

No timeline, layer model, custom renderer, or export format is required.

## Implementation slices

### Slice 0: platform spike

- Register and unregister a global shortcut.
- Capture frontmost window pixels without focusing Zeron.
- Read a bounded accessibility snapshot from Safari, Terminal, and a native
  settings window.
- Compare helper-process and in-process implementations for TCC behavior,
  packaging, latency, and crash isolation.
- Produce no permanent UI beyond diagnostic output.

Exit criterion: a clear native boundary choice backed by signed development
build behavior.

### Slice 1: local staged Appshot

- Fixed default global shortcut.
- Screen Recording permission flow.
- Screenshot plus source app/window metadata.
- Automatic routing to current composer or new-session draft.
- Distinct staged Appshot card; preview and remove.
- Local session send through existing attachment upload.

Exit criterion: shortcut in another app results in a reviewable, sendable
local Appshot without touching the paperclip flow.

### Slice 2: semantic context

- Accessibility permission and bounded tree extraction.
- Structured, escaped prompt serialization.
- Screenshot-only degradation and truncation indicators.
- Harness-level tests confirming image and context arrive together.

Exit criterion: the agent can reason about visible and off-screen application
content, with provenance and prompt-injection framing.

### Slice 3: remote and durable behavior

- Remote-host send through pending attachment transfer.
- Destination setting and recent-composer eligibility rules.
- Draft/pending-capture recovery across window restoration failures.
- Full failure and offline coverage.

### Slice 4: polish and additional platforms

- Configurable shortcut, sound, and capture transition.
- Accessibility disclosure/inspection UI.
- Windows is outside this contribution: Zeron does not yet ship a Windows app.
- Wayland portal and X11 implementations with AT-SPI enrichment.

## Verification strategy

### Pure unit tests

- destination resolution matrix;
- shortcut setting migration and conflict states;
- Appshot XML/structured serialization, escaping, and truncation;
- prompt attachment-path rewriting;
- removal keeps screenshot and semantic state coherent;
- accessibility redaction and bounded traversal.

### Integration tests

- native service result becomes composer state without submitting;
- local send supplies both image and semantic context;
- remote send resolves a pending screenshot path inside both prompt and
  attachment list;
- permission denial degrades or blocks as specified;
- Zeron is not selected as the capture target due to activation ordering.

### Manual macOS matrix

- Safari with scrolled document content;
- Terminal with scrollback;
- Xcode or another complex native application;
- multi-window application and multiple displays;
- minimized, full-screen, and transient windows;
- Screen Recording only, Accessibility only, both denied, both granted;
- local session, remote online session, and remote offline session.

## Open product decisions

1. Should Automatic ever target a session whose agent is currently running,
   where the eventual send becomes a steer?
2. Does a new-session Appshot use the existing last space/device immediately,
   or pause on the canvas until the user confirms the destination?
3. How much extracted application text should be visible before send: a status
   summary, a preview, or the full serialized snapshot?
4. Should one shortcut invocation capture only the frontmost window, or should
   holding the shortcut open a window picker in a later release?

## Recommended initial decisions

- keep macOS as the reference experience while Linux matures behind
  the same composer contract;
- screenshot-only degradation is allowed and clearly labeled;
- one shortcut invocation always captures the frontmost eligible window;
- no automatic submission;
- Automatic may stage into an idle or running selected session, but the user
  still decides whether to Send or Steer;
- a new-session capture opens the canvas and preserves existing space/device
  defaults without creating a chat until send;
- show source identity as an application icon and window title in a compact
  visual tile; keep permission/semantic status in Settings;
- use a temporary structured prompt representation before extending the RPC or
  document schemas.

## Capture feedback follow-up (2026-09-09)

macOS window selection excludes offscreen, transparent, and tiny helper windows.
When Accessibility supplies the focused window geometry, the screenshot target
must match that window; it is never replaced by a larger background document.
Without Accessibility, front-to-back visible-window selection remains available
for screenshot-only capture. The chosen window ID stays fixed during capture.

After capture and staging, a bundled macOS app requests foreground activation
through `NSWorkspace.openApplication`, with activation enabled and new-instance
creation disabled. Bare development executables retain GPUI window activation.
This runs after pixels are captured so Zeron cannot replace the source image.

Successful capture plays the original 0.67 s stereo rounded shutter and blended confirmation generated by
`scripts/generate-appshot-sound.py`. The dedicated capture sound preference and environment
kill switch apply. Capture or staging failures do not play a success cue.

API references:
- [Window visibility](https://developer.apple.com/documentation/coregraphics/kcgwindowisonscreen)
- [Accessibility geometry decoding](https://developer.apple.com/documentation/applicationservices/1462933-axvaluegetvalue)
- [Application activation](https://developer.apple.com/documentation/appkit/nsworkspace/openapplication(at:configuration:completionhandler:))

Composer tiles follow the captured window's aspect ratio at a shared 132 px
preview height. Portrait captures take less horizontal space; wider captures
expand up to a 320 px image cap and the current composer's available width.
Extreme panoramas scale down without cropping. Icons and titles keep common
baselines, and overflow remains horizontally scrollable.

### Independent capture sound

Settings → Shortcuts → Appshots includes a Capture sound toggle, persisted as
`appshotSoundEnabled`. It controls captures independently of session notification
sounds. On first load, existing settings inherit their previous `soundEnabled`
value; explicit capture preferences take precedence thereafter. The global
`ZERON_DISABLE_SOUND` override still mutes playback. The synthesized cue adapts the rounded sound-family auditions: three smooth shutter clicks 70 ms apart, then one blended C4–F4 resonance with a quiet C5 overtone. The notes share a softened attack rather than playing in two phases. The cue lasts 0.67 seconds and peaks at approximately -18.7 dBFS. Its quiet harmonic overtones decay quickly; there is no noise bed or resonant impact tail, and playback gain is never normalized upward.

### Desktop-only controls and customizable shortcut

The capture service starts only on macOS and Linux. Unsupported builds
omit the Appshots settings section and do not serialize its capture, sound,
destination or shortcut preferences. The iOS app has no Appshot capture controls.

The default remains Control–Option–Space on macOS and Control–Alt–Space on
Linux. `keymap.captureAppshot` uses the existing recorder, conflict checks,
per-row Reset and Restore defaults. Recording intercepts key events before bound
actions, suspends capture, and restores normal behavior on acceptance, rejection,
Escape, blur or page release. Changes apply to native registration without restart.

macOS resolves letters against the active ASCII-capable keyboard layout and
replaces its Carbon hotkey. X11 replaces
its passive grabs and cleans up partial registration failures. Wayland closes the
old portal session before rebinding with the preferred trigger; version 2 portals
can present their configuration UI for a changed preference. The compositor owns
the final binding, which the UI explicitly labels as a preference on Wayland.

### Capture feedback and scope follow-up

macOS acknowledges the capture immediately after staging its PNG, before the
optional accessibility traversal (which has a 900 ms deadline). The sound confirms
captured pixels; composer staging may still report its attachment-budget limit.
Capture failures before staging stay silent. Linux acknowledges successful capture
after its backend returns. The independent sound toggle and global mute still apply.

Fresh composer cards fade in and settle upward by 8 px over 240 ms using the shared
motion curve. Entity-owned timestamps prevent replay when switching away and back;
restored cards do not animate, and reduced motion bypasses the entrance.

Removed the experimental Windows backend, its Appshots-only dependencies and its
platform support claims. Existing unrelated Windows code is preserved. Linux native
portal, X11 and AT-SPI behavior still needs native validation.

### Transparent capture-surface padding

Some Chrome captures contain fully transparent outer columns in the PNG backing
surface. PNG dimensions alone then reserve a wider card than the visible window;
changing object-fit or thumbnail sizing cannot remove that invisible image area.
Before staging, Appshots now trim only fully transparent outer rows and columns,
preserve retained pixel values and color profiles, then derive dimensions from
the normalized PNG. Opaque captures remain byte-identical. Restored queue Appshots
apply the same normalization while preserving attachment identity and context.
Ordinary file attachments are not modified.

Regression coverage includes right-side backing-surface padding, rounded corners,
interior transparency, alpha=1 edges, 16-bit samples, unchanged opaque margins and
queue restoration. Saved Chrome capture validation removed 388 transparent columns
from a 3024×1654 PNG without changing any retained RGBA pixel or its ICC profile.
A Discord comparison stayed byte-identical. Existing uploaded originals are not
rewritten; older captures are normalized when restored for editing.


### Native capture-audio startup

On macOS, enabling capture sound preloads the embedded cue in AVAudioPlayer on
one dedicated worker. Successful captures request playback through a bounded
mailbox, avoiding a temporary-file write and afplay process startup per capture.
Replaying uses pause and rewind, preserving prepared resources. Native decode or
playback failure falls back to the existing system player; global mute still applies.

Success feedback still follows image staging, including transparent-padding
processing, so failed staging remains silent. Debug logs split capture acquisition,
staging and native playback-request time. These timings exclude physical output
latency (for example Bluetooth buffering); native listening remains a separate check.
