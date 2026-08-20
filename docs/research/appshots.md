# Appshots for Zeron

Status: product and architecture exploration

Branch: `wip/appshots-exploration`

Scope: desktop Zeron, macOS-first

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

App icons, sounds, and the capture-to-composer transition are useful polish but
are not part of the semantic core.

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
- Linux support in the first platform slice.
- Replacing or changing the existing paperclip, paste, or drag-and-drop flows.

### User-visible behavior

- The shortcut operates while another application is focused.
- The frontmost eligible window at shortcut time is the capture target.
- Zeron never steals focus until the screenshot has been acquired.
- A staged Appshot is visually distinct from an ordinary image attachment.
- The staged card shows the source application and window title when available.
- The user can preview the screenshot, inspect a summary of captured text,
  remove the Appshot, and type an instruction before sending.
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
- Explain why Accessibility access is needed and that it includes off-screen
  application text.
- Open the relevant macOS Settings pane.
- Re-check permission state after the user returns.
- Allow screenshot-only degraded capture if Screen Recording is granted but
  Accessibility is not; mark the missing semantic context visibly.

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
- Optionally play a completion sound; this is polish, not MVP behavior.

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

Define a small platform-neutral Rust interface in the UI crate and implement it
with a signed macOS helper initially:

```rust
trait AppshotCaptureService {
    fn register_shortcut(&self, shortcut: GlobalShortcut) -> Result<()>;
    fn permission_state(&self) -> AppshotPermissionState;
    async fn capture_frontmost_window(&self) -> Result<CapturedAppshot>;
}
```

The macOS helper is responsible for:

- global shortcut detection;
- frontmost process and window identification;
- ScreenCaptureKit window capture;
- `AXUIElement` traversal and bounded serialization;
- permission probing and Settings deep links;
- returning capture results over a narrow IPC protocol.

A helper mirrors the lifecycle and permission isolation used by mature capture
features and keeps Objective-C/Swift framework code out of GPUI state. The
trade-off is signing, packaging, helper crash handling, and a second protocol.
Before implementation, a small spike should compare this with an in-process
Rust/Objective-C bridge specifically for TCC attribution and development
ergonomics. The Rust-facing interface should remain the same either way.

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

### Slice 4: polish and second platform

- Configurable shortcut, sound, and capture transition.
- Accessibility disclosure/inspection UI.
- Windows implementation using Windows Graphics Capture and UI Automation.

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

1. Should screenshot-only capture be allowed when Accessibility permission is
   denied, or should an Appshot require both sources?
2. Should Automatic ever target a session whose agent is currently running,
   where the eventual send becomes a steer?
3. Does a new-session Appshot use the existing last space/device immediately,
   or pause on the canvas until the user confirms the destination?
4. How much extracted application text should be visible before send: a status
   summary, a preview, or the full serialized snapshot?
5. Should one shortcut invocation capture only the frontmost window, or should
   holding the shortcut open a window picker in a later release?

## Recommended initial decisions

- macOS first;
- screenshot-only degradation is allowed and clearly labeled;
- one shortcut invocation always captures the frontmost eligible window;
- no automatic submission;
- Automatic may stage into an idle or running selected session, but the user
  still decides whether to Send or Steer;
- a new-session capture opens the canvas and preserves existing space/device
  defaults without creating a chat until send;
- show source identity and a concise captured-text status in the staged card;
- use a temporary structured prompt representation before extending the RPC or
  document schemas.
