// Composer — the floating glass shell in the t3 mobile composer's shape: a
// collapsed capsule (editor + send circle) that morphs into an expanded card
// with a toolbar ROW below it (attach circle · scrolling chips · pinned send)
// when the editor takes focus. Carries the desktop's Send→Steer→Stop
// semantics: live run + text = steer (same up-arrow), live run + empty = stop.
//
// Expansion is focus-driven like t3's, with the old deterministic content
// triggers kept as a floor (attachments, newline, >26 chars) — content-size
// measurement oscillates at the boundary, so it is never measured.

import PhotosUI
import SwiftUI

/// Shared glass shell + input + action row. `chips` render in the expanded
/// toolbar row between the attach and send circles.
struct ComposerShell<Chips: View>: View {
    @Binding var draft: ComposerText
    @Binding var selection: AttributedTextSelection
    var placeholder = "Message"
    var sendEnabled: Bool
    var showStop: Bool
    var busy = false
    /// New-session composers stay expanded — the picker chips ARE the page.
    var alwaysExpanded = false
    /// Hold the expanded layout while a picker sheet is up: presenting the
    /// sheet blurs the editor, and collapsing on that blur flaps the
    /// transcript's bottom inset mid-presentation (t3 derives expansion from
    /// focus OR sheet-active for exactly this reason).
    var keepExpanded = false
    var onSend: () -> Void
    var onStop: () -> Void = {}
    /// Staged image attachments (attachment-ui.tsx AttachmentStrip inside the
    /// pill). Non-empty forces the expanded layout, like focus.
    var attachments: [StagedAttachment] = []
    /// Present the photo picker; nil hides the attach button.
    var onAttach: (() -> Void)? = nil
    var onRemoveAttachment: (String) -> Void = { _ in }
    /// Screenshot rig (-focuscomposer): take keyboard focus shortly after
    /// appearing, so the keyboard-up transcript states can be driven headless.
    var autoFocus = false
    /// Reports focus changes out, so a caller can close floating UI on blur.
    var onFocusChange: (Bool) -> Void = { _ in }
    @ViewBuilder var chips: Chips

    @FocusState private var focused: Bool
    @State private var measuredHeight: CGFloat = 22

    private var expanded: Bool {
        alwaysExpanded || keepExpanded || focused || !attachments.isEmpty
            || draft.plainText.contains("\n") || draft.plainText.count > 26
    }

    /// One animatable shape for background/glass/hairline: a true capsule
    /// collapsed, a 20pt card expanded (t3's 999↔20 morph).
    ///
    /// The collapsed radius is 999, not a literal half-height. `RoundedRectangle`
    /// clamps its radius to half the smaller dimension, so 999 is always exactly
    /// a capsule no matter how tall the pill is. The previous `24` only looked
    /// like a capsule because the pill was pinned at 46pt; once the editor's
    /// height became measured rather than fixed, 24 stopped being half of it and
    /// the pill visibly squared off.
    private var surfaceShape: RoundedRectangle {
        RoundedRectangle(cornerRadius: expanded ? 20 : 999)
    }

    // Switching between VStack/HStack via AnyLayout (rather than an if/else
    // that swaps container types) keeps `input`'s view identity stable across
    // the compact↔expanded flip — an if/else here would tear down and rebuild
    // the TextEditor, dropping keyboard focus mid-type.
    private var shellLayout: AnyLayout {
        expanded
            ? AnyLayout(VStackLayout(alignment: .leading, spacing: 0))
            : AnyLayout(HStackLayout(alignment: .center, spacing: 12))
    }

    var body: some View {
        surface
            // Focus-widen: margins pull in slightly while typing (chat-session.tsx).
            .padding(.horizontal, focused ? 10 : 16)
            .motionAnimation(Motion.resize, value: focused)
            .motionAnimation(Motion.collapse, value: expanded)
            .onAppear {
                guard autoFocus else { return }
                Task { @MainActor in
                    try? await Task.sleep(nanoseconds: 1_500_000_000)
                    focused = true
                }
            }
            .onChange(of: focused) { _, now in onFocusChange(now) }
    }

    /// The glass surface: collapsed = editor + send in one capsule row;
    /// expanded = attachment strip over a tall editor, with the control row
    /// (attach circle · scrolling chips · pinned send) along the card's
    /// bottom edge — everything stays inside the glass.
    private var surface: some View {
        shellLayout {
            if expanded, !attachments.isEmpty {
                AttachmentStripView(attachments: attachments, remove: onRemoveAttachment)
                    .padding(.bottom, 10)
            }
            input
                .padding(.leading, expanded ? 4 : 13)
                .padding(.trailing, expanded ? 4 : 0)
                .padding(.vertical, expanded ? 4 : 5)
                .frame(minHeight: expanded ? 64 : nil, alignment: .topLeading)
            if expanded {
                HStack(spacing: 8) {
                    if onAttach != nil {
                        attachButton
                    }
                    // Chips scroll; the send button stays pinned.
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 8) {
                            chips
                        }
                    }
                    .scrollClipDisabled(false)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    actionButton
                }
                .padding(.top, 8)
            } else {
                actionButton
            }
        }
        .padding(.horizontal, expanded ? 12 : 5)
        .padding(.vertical, expanded ? 12 : 5)
        .background(whiteAlpha(0.04), in: surfaceShape)
        .glassEffect(.regular.interactive(), in: surfaceShape)
        .overlay(surfaceShape.strokeBorder(whiteAlpha(0.05), lineWidth: 1))
        // The whole glass surface focuses the editor, not just the TextEditor's
        // own text box: the collapsed pill is mostly padding, and a tap that
        // misses the text box falls through to the transcript underneath —
        // whose tap-to-blur then RESIGNS the keyboard. That's the "have to
        // press a few times" miss. Buttons and chips still win their taps.
        // Masked to .subviews while focused so cursor-placement taps inside
        // the editor stay fully native.
        .contentShape(surfaceShape)
        .gesture(TapGesture().onEnded { focused = true },
                 including: focused ? .subviews : .all)
    }

    // TextEditor, not TextField: only the AttributedString overload can carry
    // mention chips (iOS 26+). It gives up three things TextField had, so each
    // is rebuilt here: the placeholder is an overlay, the 1...7 line limit is
    // an explicit height range, and the opaque background is hidden so the
    // glass shows through.
    private var input: some View {
        TextEditor(text: editorText, selection: $selection)
            .font(Theme.sans(16))
            .foregroundStyle(Theme.text)
            .tint(Theme.text)
            .attributedTextFormattingDefinition(MentionFormatting())
            .scrollContentBackground(.hidden)
            .frame(height: clampedHeight)
            .scrollDisabled(measuredHeight <= lineHeight * 7)
            .focused($focused)
            .background(alignment: .topLeading) { heightMirror }
            // Collapsed, the pill is exactly one line, so the placeholder is
            // CENTRED with no vertical offset — `.leading` does that on its own,
            // at any height. Expanded, the editor is multi-line and the
            // placeholder belongs on the first line, so it top-aligns.
            //
            // The previous `.topLeading` + `.padding(.top, 8)` was a constant
            // tuned against the old fixed editor height. Once that height became
            // measured, the constant stopped meaning "first line" and just
            // pushed the text low — which is exactly what it looked like.
            .overlay(alignment: expanded ? .topLeading : .leading) {
                if draft.isEmpty {
                    Text(placeholder)
                        .font(Theme.sans(16))
                        .foregroundStyle(Theme.textFaint)
                        .padding(.leading, 4)
                        .padding(.top, expanded ? 8 : 0)
                        .allowsHitTesting(false)
                }
            }
    }

    /// One line of the composer font, used to bound the editor's growth the way
    /// `lineLimit(1...7)` used to.
    private var lineHeight: CGFloat { 22 }

    /// Collapsed is pinned, not measured. The mirror adds its own vertical
    /// padding on top of the paddings the shell already applies, which pushed
    /// the one-line pill from its original 46pt to about 60pt — visibly taller
    /// and, with a capsule radius, visibly fatter. One line has no wrapping to
    /// discover, so there is nothing to measure: pin it and let the mirror do
    /// the job it exists for, which is growth.
    private var collapsedEditorHeight: CGFloat { 26 }

    private var clampedHeight: CGFloat {
        guard expanded else { return collapsedEditorHeight }
        return min(max(measuredHeight, lineHeight), lineHeight * 7)
    }

    /// Height measurement, taken from a hidden `Text` MIRROR rather than from
    /// the editor's own content size.
    ///
    /// This distinction is the whole point. The editor's content height depends
    /// on the frame we set from it, so reading one to set the other makes them
    /// chase each other — the oscillation this file's own comments already warn
    /// about. A mirror's height depends only on the text and the available
    /// width, never on the frame we apply to the editor, so the loop is broken.
    ///
    /// It is also what solves WRAPPING. The Task 0 spike verified 1-to-7 growth
    /// from hard newlines only and left wrapped growth unsolved. `Text` wraps at
    /// the same width with the same font, so its height IS the wrapped height.
    ///
    /// Three details here are load-bearing, and each one silently breaks growth
    /// if it is dropped:
    ///
    /// `.fixedSize(horizontal: false, vertical: true)` — `.background` proposes
    /// the PRIMARY view's size to its content, so without this the mirror is
    /// proposed `clampedHeight` and can report exactly that back, pinning
    /// `measuredHeight` at one line and freezing the editor forever. The file
    /// already uses this modifier for the same reason at the question panel.
    ///
    /// `.padding(.horizontal, 5)` — the editor wraps at its frame width MINUS
    /// its own text inset; a mirror with no inset wraps a character or two
    /// later, so at every wrap boundary the editor has a line the mirror has not
    /// counted. With `scrollDisabled` true in that window, the new line and the
    /// caret are clipped. Insetting the mirror over-measures instead, which is
    /// the harmless direction.
    ///
    /// Per-line height is over-measured too (chips render at mono 15, the mirror
    /// measures everything at sans 16), and that is also harmless. Note the
    /// width bias runs the OTHER way — mono advances are wider than the
    /// proportional average, so a chip-heavy line wraps earlier in the editor
    /// than in the mirror. The horizontal inset above is what absorbs that.
    private var heightMirror: some View {
        Text(mirroredText)
            .font(Theme.sans(16))
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 5)
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, alignment: .topLeading)
            .hidden()
            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height in
                measuredHeight = height
            }
    }

    /// What the mirror measures, with two deliberate distortions.
    ///
    /// A trailing newline gets a sentinel space. TextKit gives the editor an
    /// extra line fragment for a trailing paragraph break and SwiftUI `Text`
    /// does not, so pressing Return at the end of a draft would measure the same
    /// height as before: the frame would not grow, and the caret would sit
    /// inside a scroll-disabled clip until the next keystroke. Return is the
    /// explicit "I want another line" gesture, so this is the most visible of
    /// the mirror's failure modes.
    ///
    /// An empty draft measures a single space, so the collapsed pill is one line
    /// tall rather than zero.
    private var mirroredText: String {
        let plain = draft.plainText
        if plain.isEmpty { return " " }
        return plain.hasSuffix("\n") ? plain + " " : plain
    }

    /// THE ONLY PATH USER TYPING TAKES. `ComposerText.apply(…)` enforces the
    /// invariant for programmatic picks, but a keystroke never goes through it
    /// — the text view writes straight to the binding. Without this setter, a
    /// chip typed into would keep its attribute on device while the unit tests
    /// still passed, because those drive `apply` instead.
    ///
    /// Enforcing here rather than in an `.onChange` is deliberate: mutating the
    /// binding from inside its own change notification is the re-entrancy the
    /// `clearDraft` comment warns about. The pass settles in one step — a run
    /// that has already lost its attribute is no longer a mention run, so a
    /// second pass finds nothing to strip.
    private var editorText: Binding<AttributedString> {
        Binding(
            get: { draft.attributed },
            set: { next in
                var updated = draft
                updated.attributed = next
                updated.enforceInvariant()
                draft = updated
            }
        )
    }

    private var attachButton: some View {
        Button {
            onAttach?()
        } label: {
            Image(systemName: "plus")
                .font(.system(size: 16, weight: .medium))
                .foregroundStyle(Theme.textMuted)
                .frame(width: 40, height: 40)
                .background(whiteAlpha(0.06), in: Circle())
                .overlay(Circle().strokeBorder(whiteAlpha(0.08), lineWidth: 1))
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .disabled(busy)
    }

    /// Attachments count as content: an image-only send is a send, never a stop.
    private var hasContent: Bool {
        !draft.plainText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || !attachments.isEmpty
    }

    private var actionButton: some View {
        Button {
            if showStop, !hasContent {
                UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                onStop()
            } else {
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                onSend()
            }
        } label: {
            Group {
                if busy {
                    ProgressView()
                        .controlSize(.small)
                        .tint(Theme.bg)
                } else if showStop, !hasContent {
                    RoundedRectangle(cornerRadius: 3.5)
                        .fill(Theme.bg)
                        .frame(width: 12, height: 12)
                } else {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 16, weight: .semibold))
                        .foregroundStyle(buttonActive ? Theme.bg : Theme.textFaint)
                }
            }
            .frame(width: 40, height: 40)
            .background(buttonActive ? AnyShapeStyle(Theme.text) : AnyShapeStyle(whiteAlpha(0.10)),
                        in: Circle())
            .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .disabled(!buttonActive)
        .motionAnimation(Motion.fadeQuick, value: showStop)
    }

    private var buttonActive: Bool {
        if showStop, !hasContent { return true }
        return sendEnabled && hasContent && !busy
    }
}

/// The live-chat composer: input, the photo attach button, the model + trait
/// picker chips (harness stays locked mid-chat; picks merge into the chat's
/// config row for the next dispatch), and the morphing action button.
struct ComposerView: View {
    @Environment(AppModel.self) private var model
    let store: SessionStore
    let chat: Chat
    let runLive: Bool

    @State private var text = ComposerText()
    @State private var selection = AttributedTextSelection()
    @State private var suggestions = ComposerSuggestions()
    @State private var attachments: [StagedAttachment] = []
    @State private var pickerItems: [PhotosPickerItem] = []
    @State private var showPicker = false
    @State private var uploading = false
    @State private var uploadProgress: Double?
    @State private var uploadError: String?
    @State private var showModelPicker = false
    @State private var showTraitPicker = false
    /// Live catalog for the chat's harness from its space's device.
    @State private var catalogs: [String: [ModelInfo]] = [:]
    /// Mirrors ComposerShell's own `@FocusState`, reported out through
    /// `onFocusChange`. Gates the popover: on desktop, tap-outside blurs the
    /// editor AND dismisses the mention popup in the same gesture
    /// (crates/ui/src/composer.rs:3966). SwiftUI has no equivalent of
    /// `on_mouse_down_out`, so blur is the signal this reaches for instead —
    /// a tap on the transcript resigns focus, and the popover must follow it
    /// down rather than floating over a dismissed keyboard.
    @State private var composerFocused = false

    private var harness: String { chat.config?.harness ?? "claude-code" }

    private var models: [ModelInfo] {
        catalogs[harness] ?? HarnessCatalog.models(for: harness)
    }

    private var currentModel: ModelInfo {
        models.first { $0.id == chat.config?.model } ?? HarnessCatalog.defaultModel(for: harness)
    }

    private var currentReasoning: String? {
        guard !currentModel.reasoningLevels.isEmpty else { return nil }
        if let r = chat.config?.reasoning, currentModel.reasoningLevels.contains(r) { return r }
        return HarnessCatalog.defaultReasoning(for: currentModel)
    }

    var body: some View {
        // Subscribe to the connectivity pulse so the degraded caption follows
        // the graced stream (1Hz only while something is degraded/pending).
        let _ = model.connectivity.pulse
        return VStack(spacing: 6) {
            if let uploadError {
                Text(uploadError)
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.danger)
                    .lineLimit(2)
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if uploading, let uploadProgress {
                Text("Uploading… \(Int(uploadProgress * 100))%")
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textMuted)
                    .monospacedDigit()
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            // Pre-send honesty (composer.rs degraded notice, post-#170 copy):
            // one quiet caption line, never a warning box — the send still
            // works, it just queues.
            if model.chatDeliveryDegraded(chat) {
                Text(model.connectivity.state == .offline
                    ? "Offline — messages will send when you're back online."
                    : "Messages will send once the connection recovers.")
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textFaint)
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .transition(.opacity)
            }
            // Last child before the pill so the popover sits directly on top of
            // it: a caption wedged in between would break the two glass
            // surfaces apart.
            //
            // Gate is trigger-open AND focused, nothing about `items` or
            // `errorText` or `isLoading`. Focus is required so a tap on the
            // transcript — which blurs the editor without changing the draft,
            // so the trigger alone would still match — closes the popover the
            // same way desktop's `.on_mouse_down_out(… dismiss_mention …)`
            // does (crates/ui/src/composer.rs:3966). Items/error/loading used
            // to gate visibility too, but that left the zero-match empty state
            // ("No matching commands.") unreachable — with only a trigger and
            // focus needed, `scheduleRefresh`'s `beginPending()` call keeps
            // `isLoading` true through the whole 80ms debounce window, so the
            // popover shows "Searching files…" rather than flashing the
            // empty-state line before the first request has even gone out.
            //
            // A SIBLING in this VStack, not a ZStack over ComposerShell:
            // ZStack(alignment: .bottom) aligns both children's bottom edges and
            // draws the later one (the pill) in front, so the pill would cover the
            // popover completely at one to three rows and swallow taps on every
            // covered row through its own contentShape. The VStack's 6pt spacing
            // is the gap the two glass surfaces want anyway.
            if let trigger = text.trigger(at: selection), composerFocused {
                ComposerPopover(items: suggestions.items,
                                kind: trigger.kind,
                                isLoading: suggestions.isLoading,
                                errorText: suggestions.errorText) { item in
                    pick(item, over: trigger.range)
                }
                // 10, not 16: ComposerShell narrows its own margins to 10 while
                // focused (ComposerView.swift:68), and the popover only ever shows
                // while focused. 16 would leave its edges visibly inset from the
                // pill directly beneath it.
                .padding(.horizontal, 10)
                .transition(.opacity)
            }
            ComposerShell(
                draft: $text,
                selection: $selection,
                sendEnabled: true,
                showStop: runLive,
                busy: uploading,
                keepExpanded: showModelPicker || showTraitPicker,
                onSend: send,
                onStop: { store.sendInterrupt() },
                attachments: attachments,
                onAttach: { showPicker = true },
                onRemoveAttachment: { id in attachments.removeAll { $0.id == id } },
                autoFocus: model.launchFocusComposer,
                onFocusChange: { composerFocused = $0 }
            ) {
                if let pullRequest = model.changeRequest(for: chat) {
                    PullRequestBadge(summary: pullRequest, surface: .composer)
                }
                if let branch = chat.branch?.trimmingCharacters(in: .whitespacesAndNewlines),
                   !branch.isEmpty {
                    BranchContextChip(branch: branch)
                }
                ComposerChip(label: currentModel.label, badgeHarness: harness) {
                    showModelPicker = true
                }
                if let currentReasoning {
                    ComposerChip(label: HarnessCatalog.reasoningLabel(currentReasoning)) {
                        showTraitPicker = true
                    }
                }
            }
        }
        .photosPicker(isPresented: $showPicker, selection: $pickerItems,
                      maxSelectionCount: 8, matching: .images)
        .onChange(of: pickerItems) { _, items in
            guard !items.isEmpty else { return }
            stage(items)
        }
        .sheet(isPresented: $showModelPicker) {
            ModelPickerSheet(
                harness: .constant(harness),
                modelId: Binding(
                    get: { currentModel.id },
                    set: { writeConfig(model: $0, reasoning: chat.config?.reasoning) }
                ),
                reasoning: Binding(
                    get: { chat.config?.reasoning },
                    set: { writeConfig(model: chat.config?.model, reasoning: $0) }
                ),
                lockedHarness: true,
                catalogs: catalogs
            )
        }
        .sheet(isPresented: $showTraitPicker) {
            TraitPickerSheet(
                reasoning: Binding(
                    get: { currentReasoning },
                    set: { writeConfig(model: chat.config?.model, reasoning: $0) }
                ),
                levels: currentModel.reasoningLevels
            )
        }
        .task(id: "\(chat.id)/\(harness)") {
            // Wire the fetchers FIRST, before the guard and before any await.
            // `listModels` is an RPC to the host and can stall for seconds when
            // the device is offline. If the user types `/` in that window, the
            // default no-op fetcher returns [] and ComposerSuggestions caches
            // that empty list under the real key — permanently, for the life of
            // the view. Every later `/` then silently shows nothing.
            //
            // Both fetchers go through AppModel, not straight to the store, so
            // demo mode answers from its own dataset instead of returning [].
            suggestions.fetchCommands = { [weak model] harness, device, cwd in
                guard let model else { return [] }
                return try await model.listCommands(deviceId: device, harness: harness, cwd: cwd)
            }
            suggestions.fetchPaths = { [weak model] context, query in
                guard let model else { return [] }
                return try await model.searchFiles(deviceId: context.deviceId,
                                                   chatId: context.chatId,
                                                   spaceId: context.spaceId,
                                                   query: query)
            }

            guard let space = model.space(for: chat) else { return }
            catalogs[harness] = await model.listModels(space: space, harness: harness)
        }
        .onChange(of: text) { _, _ in scheduleRefresh() }
        .onChange(of: selection) { _, _ in scheduleRefresh() }
        .onAppear {
            if model.launchSheet == "config" {
                model.launchSheet = nil
                showModelPicker = true
            }
        }
    }

    /// Everything the fetchers need. The chat's own device hosts the run, so it
    /// is the device to dial: a `chatId` search only works there
    /// (crates/engine/src/rpc.rs:501-502) — `chat.deviceId`, not the space's,
    /// is the authority for that, and the engine rejects a `chatId` search
    /// whose chat does not belong to the dialed device ("chat belongs to
    /// another device"). Reading it off `space.deviceId` disabled the whole
    /// feature for any space-less chat, since `space` is Optional.
    private var suggestionContext: SuggestionContext {
        SuggestionContext(harness: harness,
                         deviceId: chat.deviceId,
                         cwd: slashCwd(chatCwd: chat.cwd,
                                       spacePath: model.space(for: chat)?.path),
                         chatId: chat.id,
                         spaceId: nil)
    }

    private func pick(_ item: SuggestionItem, over range: Range<Int>) {
        switch item.payload {
        case .command(let name):
            text.apply(command: name, over: range, selection: &selection)
        case .path(let path, let isDir):
            text.apply(path: path, isDir: isDir, over: range, selection: &selection)
        }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    private func refreshSuggestions() async {
        guard let trigger = text.trigger(at: selection) else { return }
        await suggestions.update(trigger: trigger, context: suggestionContext)
    }

    @State private var refreshTask: Task<Void, Never>?

    /// Debounce and cancel, for two reasons. One keystroke changes BOTH `text`
    /// and `selection`, so without this every character fires two refreshes and
    /// two `SearchFiles` RPCs, one of which the generation guard always throws
    /// away. And an un-cancelled `Task` per keystroke lets a fast typist queue
    /// an unbounded number of them. The desktop debounces the same way
    /// (crates/ui/src/composer.rs:3871-3876).
    ///
    /// The trigger check runs SYNCHRONOUSLY here, before any debounce, for two
    /// reasons that both have to happen on this exact keystroke rather than
    /// 80ms later:
    ///   - No trigger: `reset()` drops any stale rows from a trigger that just
    ///     closed, so a later trigger of the same kind can't reopen onto the
    ///     previous query's results while the new fetch is in flight.
    ///   - A trigger: `beginPending()` marks the fetch pending immediately, so
    ///     the popover's "Loading…" / "Searching files…" state covers the
    ///     debounce window too, and the empty-state line never flashes between
    ///     the keystroke and the first request going out.
    private func scheduleRefresh() {
        refreshTask?.cancel()
        guard text.trigger(at: selection) != nil else {
            suggestions.reset()
            return
        }
        suggestions.beginPending()
        refreshTask = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(80))
            guard !Task.isCancelled else { return }
            await refreshSuggestions()
        }
    }

    /// Merge a model/effort change into the chat's config row (LWW; the host
    /// picks it up on the next run dispatch). Copies preserve modelOptions.
    private func writeConfig(model newModel: String?, reasoning newReasoning: String?) {
        var config = chat.config ?? ChatConfig(harness: harness, model: nil,
                                               reasoning: nil, sandbox: "workspace-write")
        config.model = newModel
        config.reasoning = newReasoning
        model.setChatConfig(chatId: chat.id, config: config)
    }

    /// Load picked photos into staged attachments (HEIC transcodes to JPEG;
    /// unsupported/oversized picks surface as an error line).
    private func stage(_ items: [PhotosPickerItem]) {
        Task { @MainActor in
            var failed = 0
            for item in items {
                guard let data = try? await item.loadTransferable(type: Data.self),
                      let staged = StagedAttachment.stage(data: data) else {
                    failed += 1
                    continue
                }
                attachments.append(staged)
            }
            pickerItems = []
            if failed > 0 {
                uploadError = failed == 1
                    ? "One image couldn't be attached (unsupported or over 24 MB)."
                    : "\(failed) images couldn't be attached (unsupported or over 24 MB)."
            } else {
                uploadError = nil
            }
        }
    }

    private func send() {
        let prompt = text.markdown().trimmingCharacters(in: .whitespacesAndNewlines)
        let staged = attachments
        guard !prompt.isEmpty || !staged.isEmpty else { return }

        if staged.isEmpty {
            deliver(content: prompt, paths: [])
            clearDraft()
            return
        }
        if model.hostSupportsQueuedAttachments(chat) {
            // Queued flow (host ≥ 0.2.12): the send is a durable local write
            // NOW — pending:// refs in the doc, bytes escorted afterwards
            // with retry-forever — so an image send survives a dead link
            // exactly like a text send does.
            let transfers = staged.map {
                AttachmentTransfer(uploadId: UUID().uuidString.lowercased(),
                                   name: $0.name, data: $0.data)
            }
            for (transfer, att) in zip(transfers, staged) {
                // Seed under the pending ref — the echo's thumbnail renders
                // from local bytes while the upload crosses the relay.
                AttachmentImageCache.shared.seed(
                    deviceId: chat.deviceId,
                    path: UploadStash.pendingRef(uploadId: transfer.uploadId, name: transfer.name),
                    name: att.name, data: att.data)
            }
            store.sendWithTransfers(prompt: prompt, chat: chat, live: runLive,
                                    transfers: transfers)
            attachments = []
            clearDraft()
            return
        }
        // Legacy host-staged flow (host < 0.2.12): upload first, send after —
        // the refs trailer needs the committed paths. Bounded by the
        // whole-attachment deadline; progress narrates instead of a bare
        // spinner (a lawful crawl must never read as a hang).
        uploading = true
        uploadError = nil
        uploadProgress = 0
        let progressBinding = $uploadProgress
        Task { @MainActor in
            defer {
                uploading = false
                uploadProgress = nil
            }
            do {
                var paths: [String] = []
                let total = staged.count
                for (ix, att) in staged.enumerated() {
                    let path = try await store.uploadAttachment(name: att.name, data: att.data) { fraction in
                        progressBinding.wrappedValue = (Double(ix) + fraction) / Double(total)
                    }
                    // Seed the cache so our own bubble renders from local
                    // bytes instead of a round-trip.
                    AttachmentImageCache.shared.seed(deviceId: chat.deviceId, path: path,
                                                     name: att.name, data: att.data)
                    paths.append(path)
                }
                deliver(content: withAttachments(text: prompt, paths: paths), paths: paths)
                attachments = []
                clearDraft()
            } catch {
                uploadError = "Attachment upload failed — \(error.localizedDescription)"
            }
        }
    }

    private func deliver(content: String, paths: [String]) {
        if runLive {
            store.sendSteer(prompt: content)
        } else {
            store.sendRun(prompt: content, chat: chat, attachments: paths)
        }
    }

    private func clearDraft() {
        text.clear(selection: &selection)
        // The clear above is unconditional, so a prompt left sitting in the
        // composer after a successful send is not this path failing to run —
        // it is the text view writing the pre-send string back. A focused
        // multiline editor commits pending autocorrect/marked text through
        // the binding AFTER a programmatic change, which restores the prompt.
        // Re-clear once that has drained; a keystroke can't land inside the
        // same main-actor turn, so this can never eat real input.
        Task { @MainActor in text.clear(selection: &selection) }
    }
}

// MARK: - Question panel (composer.rs Wizard)

struct QuestionPanel: View {
    let requestId: String
    let questions: [UserInputQuestion]
    let respond: (String, [UserInputAnswer]) -> Void

    @State private var page = 0
    @State private var picked: [String: Set<String>] = [:]  // questionId → labels
    @State private var typed: [String: String] = [:]
    @State private var autoAdvanceTask: Task<Void, Never>?

    var body: some View {
        // `questions[min(page, count - 1)]` traps on an empty list (count - 1
        // is -1). A request whose questions fail to decode reaches here empty,
        // so this crashed the app on any session holding one.
        if questions.isEmpty {
            EmptyView()
        } else {
            panel(for: questions[min(max(page, 0), questions.count - 1)])
        }
    }

    private func panel(for question: UserInputQuestion) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(question.header.uppercased())
                    .font(Theme.sans(10.5, weight: .medium))
                    .kerning(1)
                    .foregroundStyle(Theme.textMuted.opacity(0.6))
                Spacer()
                if questions.count > 1 {
                    Text("\(page + 1)/\(questions.count)")
                        .font(Theme.sans(10))
                        .foregroundStyle(Theme.textMuted)
                        .padding(.horizontal, 6)
                        .frame(height: 20)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 6))
                }
            }

            Text(question.question)
                .font(Theme.sans(15, weight: .medium))
                .foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)

            if question.multiSelect == true {
                Text("Select one or more options.")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textMuted)
            }

            VStack(spacing: 4) {
                ForEach(Array(question.options.enumerated()), id: \.offset) { ix, option in
                    optionRow(question: question, ix: ix, option: option)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Rectangle().fill(whiteAlpha(0.06)).frame(height: 1)
                TextField("Or type your own answer", text: Binding(
                    get: { typed[question.id] ?? "" },
                    set: { typed[question.id] = $0 }
                ))
                .font(Theme.sans(13))
                .foregroundStyle(Theme.text)
                .padding(.top, 6)
            }

            HStack {
                if page > 0 {
                    Button("Back") {
                        page -= 1
                    }
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.textMuted)
                }
                Spacer()
                Button(page < questions.count - 1 ? "Next" : "Submit") {
                    advance()
                }
                .font(Theme.sans(13, weight: .medium))
                .foregroundStyle(Theme.bg)
                .padding(.horizontal, 16)
                .frame(height: 34)
                .background(Theme.text, in: Capsule())
                .opacity(canAdvance(question) ? 1 : 0.4)
                .disabled(!canAdvance(question))
            }
        }
        .padding(16)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: 26))
        .overlay(RoundedRectangle(cornerRadius: 26).strokeBorder(whiteAlpha(0.05), lineWidth: 1))
        .padding(.horizontal, 12)
        .transition(.opacity)
    }

    private func optionRow(question: UserInputQuestion, ix: Int, option: String) -> some View {
        let isPicked = (typed[question.id] ?? "").isEmpty
            && picked[question.id, default: []].contains(option)
        return Button {
            pick(question: question, option: option)
        } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(option)
                        .font(Theme.sans(13.5, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .multilineTextAlignment(.leading)
                }
                Spacer(minLength: 0)
                if ix < 9 {
                    Text("\(ix + 1)")
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textMuted)
                        .frame(width: 22, height: 22)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 6))
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(isPicked ? whiteAlpha(0.09) : whiteAlpha(0.025),
                        in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12)
                .strokeBorder(isPicked ? whiteAlpha(0.16) : .clear, lineWidth: 1))
        }
        .buttonStyle(.plain)
    }

    private func pick(question: UserInputQuestion, option: String) {
        typed[question.id] = nil
        if question.multiSelect == true {
            var set = picked[question.id, default: []]
            if set.contains(option) { set.remove(option) } else { set.insert(option) }
            picked[question.id] = set
        } else {
            picked[question.id] = [option]
            // Single-select auto-advances after 220ms (AUTO_ADVANCE_MS).
            autoAdvanceTask?.cancel()
            autoAdvanceTask = Task {
                try? await Task.sleep(nanoseconds: 220_000_000)
                guard !Task.isCancelled else { return }
                advance()
            }
        }
    }

    private func canAdvance(_ question: UserInputQuestion) -> Bool {
        !(typed[question.id] ?? "").isEmpty || !picked[question.id, default: []].isEmpty
    }

    private func advance() {
        let question = questions[min(page, questions.count - 1)]
        guard canAdvance(question) else { return }
        if page < questions.count - 1 {
            page += 1
            return
        }
        let answers = questions.map { q -> UserInputAnswer in
            let typedAnswer = (typed[q.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if !typedAnswer.isEmpty {
                return UserInputAnswer(questionId: q.id, labels: [typedAnswer])
            }
            return UserInputAnswer(questionId: q.id, labels: Array(picked[q.id, default: []]))
        }
        respond(requestId, answers)
    }
}
