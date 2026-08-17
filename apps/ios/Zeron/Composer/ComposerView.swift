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
    @Binding var draft: String
    var placeholder = "Message"
    var sendEnabled: Bool
    /// Treat an empty box as sendable. Only the queue-edit path wants this:
    /// emptying a queued message is how you throw it away, so the button has
    /// to stay live with nothing typed.
    var allowEmptySend = false
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
    @ViewBuilder var chips: Chips

    @FocusState private var focused: Bool

    private var expanded: Bool {
        alwaysExpanded || keepExpanded || focused || !attachments.isEmpty
            || draft.contains("\n") || draft.count > 26
    }

    /// One animatable shape for background/glass/hairline: capsule-radius
    /// collapsed (46pt tall pill), 20pt card expanded (t3's 999↔20 morph).
    private var surfaceShape: RoundedRectangle {
        RoundedRectangle(cornerRadius: expanded ? 20 : 24)
    }

    // Switching between VStack/HStack via AnyLayout (rather than an if/else
    // that swaps container types) keeps `input`'s view identity stable across
    // the compact↔expanded flip — an if/else here would tear down and rebuild
    // the TextField, dropping keyboard focus mid-type.
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
        // The whole glass surface focuses the editor, not just the TextField's
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

    private var input: some View {
        TextField(placeholder, text: $draft, axis: .vertical)
            .font(Theme.sans(16))
            .foregroundStyle(Theme.text)
            .tint(Theme.text)
            .lineLimit(1...7)
            .focused($focused)
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
        allowEmptySend || !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
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

    @State private var text = ""
    @State private var attachments: [StagedAttachment] = []
    @State private var pickerItems: [PhotosPickerItem] = []
    @State private var showPicker = false
    @State private var uploading = false
    @State private var uploadError: String?
    @State private var showModelPicker = false
    @State private var showTraitPicker = false
    /// Live catalog for the chat's harness from its space's device.
    @State private var catalogs: [String: [ModelInfo]] = [:]
    /// The queued row being retyped in the box, if any.
    @State private var editingQueuedId: String?
    /// Whatever was half-typed when that edit started, put back afterwards.
    @State private var stashedDraft: String?

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
        VStack(spacing: 6) {
            if let uploadError {
                Text(uploadError)
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.danger)
                    .lineLimit(2)
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            // What is waiting to be sent, stacked directly above the box it
            // was typed in.
            if !store.queue.isEmpty {
                QueuePanel(store: store,
                           editingId: editingQueuedId,
                           onEdit: beginQueueEdit,
                           onCancelEdit: cancelQueueEdit)
            }
            ComposerShell(
                draft: $text,
                placeholder: editingQueuedId == nil ? "Message" : "Edit queued message",
                // Saving an edit down to nothing is how a queued message is
                // thrown away, so the button stays live on an empty box.
                sendEnabled: true,
                allowEmptySend: editingQueuedId != nil,
                showStop: runLive,
                busy: uploading,
                keepExpanded: showModelPicker || showTraitPicker,
                onSend: send,
                onStop: { store.sendInterrupt() },
                attachments: attachments,
                onAttach: { showPicker = true },
                onRemoveAttachment: { id in attachments.removeAll { $0.id == id } },
                autoFocus: model.launchFocusComposer
            ) {
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
            guard let space = model.space(for: chat) else { return }
            catalogs[harness] = await model.listModels(space: space, harness: harness)
        }
        .onAppear {
            if model.launchSheet == "config" {
                model.launchSheet = nil
                showModelPicker = true
            }
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

    /// Retype a queued message in the composer. Whatever was already in the box
    /// steps aside rather than being lost.
    private func beginQueueEdit(_ item: QueuedMessage) {
        if editingQueuedId == nil { stashedDraft = text }
        editingQueuedId = item.id
        text = item.text
    }

    private func cancelQueueEdit() {
        guard editingQueuedId != nil else { return }
        editingQueuedId = nil
        text = stashedDraft ?? ""
        stashedDraft = nil
    }

    /// Save the retyped row. Saving it empty is how a queued message is thrown
    /// away — nothing to send is a decision, not an error.
    private func commitQueueEdit(_ id: String) {
        store.updateQueued(id: id, text: text.trimmingCharacters(in: .whitespacesAndNewlines))
        editingQueuedId = nil
        text = stashedDraft ?? ""
        stashedDraft = nil
    }

    private func send() {
        // Editing a queued row: the button saves that row rather than sending
        // anything new.
        if let editingQueuedId {
            commitQueueEdit(editingQueuedId)
            return
        }
        let prompt = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let staged = attachments
        guard !prompt.isEmpty || !staged.isEmpty else { return }

        if staged.isEmpty {
            deliver(content: prompt, paths: [])
            clearDraft()
            return
        }
        // Upload first, send after: the refs trailer needs the committed
        // paths, and the doc entry must never point at files that don't
        // exist. The shell shows the spinner (`busy`) while chunks stream.
        uploading = true
        uploadError = nil
        Task { @MainActor in
            defer { uploading = false }
            do {
                var paths: [String] = []
                for att in staged {
                    let path = try await store.uploadAttachment(name: att.name, data: att.data)
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
        // Mid-turn, the message joins the queue rather than racing the turn:
        // whether it then steers into this turn or waits for the next one is
        // the host's call (DocHost::drain_queue). It sits in the panel above
        // meanwhile, editable, which beats a bubble that looks sent.
        if runLive {
            store.enqueueMessage(text: content, attachments: paths)
        } else {
            store.sendRun(prompt: content, chat: chat, attachments: paths)
        }
    }

    private func clearDraft() {
        text = ""
        // The clear above is unconditional, so a prompt left sitting in the
        // composer after a successful send is not this path failing to run —
        // it is the text view writing the pre-send string back. A focused
        // multiline TextField commits pending autocorrect/marked text through
        // the binding AFTER a programmatic change, which restores the prompt.
        // Re-clear once that has drained; a keystroke can't land inside the
        // same main-actor turn, so this can never eat real input.
        Task { @MainActor in text = "" }
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
