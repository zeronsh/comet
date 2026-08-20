// New session — a real composer page, not a form. Mirrors the old mobile
// app's canvas (faded mark + "What are we building?" + glass composer with
// picker chips) and the desktop's new-session canvas (composer expanded with
// in-pill pickers). The space already fixes device + folder; the composer
// carries the agent/model chip, and sending mints the chat, queues the first
// run, and swaps straight into the live session.

import PhotosUI
import SwiftUI

struct NewSessionView: View {
    @Environment(AppModel.self) private var model
    let spaceId: String
    @Binding var path: [Route]

    // Sticky run config (the old app persisted these to prefs.db).
    @AppStorage("newSessionHarness") private var harness = "claude-code"
    @AppStorage("newSessionModel") private var storedModel = ""
    @AppStorage("newSessionReasoning") private var storedReasoning = ""

    @State private var draft = ComposerText()
    @State private var selection = AttributedTextSelection()
    @State private var suggestions = ComposerSuggestions()
    @State private var showPicker = false
    @State private var showTraitPicker = false
    @State private var showRefPicker = false
    @State private var showCheckoutPicker = false
    @State private var attachments: [StagedAttachment] = []
    @State private var pickerItems: [PhotosPickerItem] = []
    @State private var showPhotoPicker = false
    @State private var attachError: String?
    /// Live harness list from the space's device (Settings → Agents gate);
    /// static pair until it loads.
    @State private var liveHarnesses: [HarnessInfo]?
    /// Live per-harness catalogs from the space's device (static fallback).
    @State private var catalogs: [String: [ModelInfo]] = [:]
    @State private var refs: [RepoRef] = []
    @State private var selectedRef: String?
    @State private var checkoutKind: CheckoutKind = .local
    @State private var busy = false
    /// Mirrors ComposerShell's own focus, reported out through
    /// `onFocusChange`. Gates the popover for the same reason it does in a live
    /// chat (ComposerView.swift): a tap on the canvas resigns focus without
    /// changing the draft, so the trigger alone would hold the popover open
    /// over a dismissed keyboard.
    @State private var composerFocused = false
    @State private var refreshTask: Task<Void, Never>?
    /// Basis for the leading header's fixed width (SessionView's pattern —
    /// iOS 26 proposes leading toolbar items almost nothing).
    @State private var viewWidth: CGFloat = 0

    private var space: Space? {
        model.spaces.first { $0.id == spaceId }
    }

    private var harnesses: [HarnessInfo] {
        liveHarnesses ?? HarnessCatalog.harnesses
    }

    private var models: [ModelInfo] {
        catalogs[harness] ?? HarnessCatalog.models(for: harness)
    }

    private var selectedModel: ModelInfo {
        models.first { $0.id == storedModel } ?? models[0]
    }

    private var reasoning: String? {
        if selectedModel.reasoningLevels.isEmpty { return nil }
        if selectedModel.reasoningLevels.contains(storedReasoning) { return storedReasoning }
        return HarnessCatalog.defaultReasoning(for: selectedModel)
    }

    var body: some View {
        VStack(spacing: 0) {
            // Canvas — tap dismisses the keyboard, like the old app.
            //
            // The mark and the line under it are decoration, and they hold a
            // ~126pt floor under this row. The suggestion popover is the one
            // thing on this page tall enough to need that floor back, so it
            // takes it: with the keyboard up there is not enough room for both
            // on a small device, and the loser of that fight was the whole
            // column (the canvas overflowed over the navigation bar and the
            // pill's chip row went under the keyboard). Empty, the row is a
            // plain colour and compresses to nothing.
            ZStack {
                Theme.bg
                if !suggestionsOpen {
                    VStack(spacing: 24) {
                        ZeronMark()
                            .frame(width: 84, height: 84)
                            .opacity(0.22)
                        Text("What are we building?")
                            .font(Theme.sans(15))
                            .foregroundStyle(Theme.textFaint)
                    }
                }
            }
            // Belt and braces for the frame the decoration is mid-fade on: a
            // ZStack centres its content and lets it spill both ways, and this
            // row's neighbour is the navigation bar.
            .clipped()
            .contentShape(Rectangle())
            .onTapGesture { dismissKeyboard() }

            if let space, !model.deviceOnline(space.deviceId), model.demo == nil {
                offlineNotice(space: space)
            }

            // Where-it-runs scope row (checkout + base ref), left-aligned
            // above the composer — the composer pill keeps only the agent chip.
            if space?.gitDetected == true {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        chip(icon: checkoutIcon, label: checkoutLabel) {
                            dismissKeyboard()
                            showCheckoutPicker = true
                        }
                        chip(icon: .gitBranch, label: refLabel) {
                            dismissKeyboard()
                            showRefPicker = true
                        }
                    }
                    .padding(.horizontal, 16)
                }
                .padding(.bottom, 8)
            }

            composer
                .padding(.bottom, 8)
                // The composer is served its ideal height BEFORE the canvas
                // above it. Both rows are flexible — the canvas is a bare
                // colour, and the popover's list is a scroll view that will
                // take any height up to its cap — and a VStack splits the free
                // space between flexible children rather than filling one
                // first. With the keyboard up that split starved the popover
                // to a single clipped row while the canvas above it sat empty
                // (user report). Priority reverses the order: the popover
                // takes what it wants, the canvas keeps the rest, and the
                // popover only compresses once the canvas has nothing left.
                .layoutPriority(1)
        }
        .background(Theme.bg.ignoresSafeArea())
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { viewWidth = $0 }
        .navigationTitle("New session")  // feeds the back menu
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(removing: .title)  // the leading header owns the bar
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                VStack(alignment: .leading, spacing: 1) {
                    Text("New session")
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text)
                    if let space {
                        Text("\(space.displayName) · \(model.deviceName(space.deviceId))")
                            .font(Theme.sans(10.5))
                            .foregroundStyle(Theme.textMuted.opacity(0.6))
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                // 170: enough slack that the bar never evicts the item into
                // the "…" overflow (SessionView.headerChromeInset).
                .frame(width: max(140, viewWidth - 170), alignment: .leading)
            }
            // Bare text on the bar, not a glass capsule.
            .sharedBackgroundVisibility(.hidden)
        }
        .sheet(isPresented: $showRefPicker) {
            RefPickerSheet(refs: refs, selected: selectedRef) { ref in
                await pickRef(ref)
            }
        }
        .sheet(isPresented: $showCheckoutPicker) {
            CheckoutPickerSheet(kind: checkoutKind,
                                selectedRefHasWorktree: selectedRefRow?.worktreePath != nil) { kind in
                pickCheckout(kind)
            }
        }
        .task(id: spaceId) {
            // Load refs for the branch chip (git spaces only).
            guard let space, space.gitDetected else { return }
            if let loaded = await model.listRefs(space: space) {
                refs = loaded
                if selectedRef == nil {
                    selectedRef = loaded.first(where: \.current)?.name ?? loaded.first?.name
                }
            }
        }
        .task(id: spaceId) {
            // Live harness list + a model catalog per harness, all from the
            // device that will run the session (the picker shows one sectioned
            // list across harnesses, so it needs every catalog up front).
            guard let space else { return }
            let list = await model.listHarnesses(space: space)
            liveHarnesses = list
            if !list.contains(where: { $0.id == harness }), let first = list.first {
                harness = first.id
            }
            await withTaskGroup(of: (String, [ModelInfo]).self) { group in
                for h in list {
                    group.addTask { (h.id, await model.listModels(space: space, harness: h.id)) }
                }
                for await (id, catalog) in group {
                    catalogs[id] = catalog
                }
            }
        }
        .sheet(isPresented: $showPicker) {
            ModelPickerSheet(harness: $harness, modelId: Binding(
                get: { selectedModel.id },
                set: { storedModel = $0 }
            ), reasoning: Binding(
                get: { reasoning },
                set: { storedReasoning = $0 ?? "" }
            ), harnesses: harnesses, catalogs: catalogs)
        }
        .sheet(isPresented: $showTraitPicker) {
            TraitPickerSheet(reasoning: Binding(
                get: { reasoning },
                set: { storedReasoning = $0 ?? "" }
            ), levels: selectedModel.reasoningLevels)
        }
        .photosPicker(isPresented: $showPhotoPicker, selection: $pickerItems,
                      maxSelectionCount: 8, matching: .images)
        .onChange(of: pickerItems) { _, items in
            guard !items.isEmpty else { return }
            stage(items)
        }
        .task {
            // Wire the fetchers FIRST, before any await. The default no-op
            // fetcher returns [], and ComposerSuggestions caches that empty
            // list under the real key for the life of the view — every later
            // `/` would then silently show nothing (ComposerView.swift).
            //
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
        }
        .onChange(of: draft) { _, _ in scheduleRefresh() }
        .onChange(of: selection) { _, _ in scheduleRefresh() }
        .onAppear {
            if model.launchAutosend {
                model.launchAutosend = false
                draft = ComposerText("Sketch the plan for porting the diff pane.")
                Task { @MainActor in
                    try? await Task.sleep(nanoseconds: 800_000_000)
                    send()
                }
            }
        }
    }

    // MARK: Composer

    private var composer: some View {
        VStack(spacing: 6) {
            if let attachError {
                Text(attachError)
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.danger)
                    .lineLimit(2)
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            // A SIBLING above the pill, not a ZStack over it, and gated on
            // trigger-plus-focus only — same shape and same reasons as the live
            // chat's popover (ComposerView.swift).
            if let trigger = draft.trigger(at: selection), suggestionsOpen {
                ComposerPopover(items: suggestions.items,
                                kind: trigger.kind,
                                isLoading: suggestions.isLoading,
                                errorText: suggestions.errorText) { item in
                    pick(item, over: trigger.range)
                }
                .padding(.horizontal, 10)
                .transition(.opacity)
            }
            ComposerShell(
                draft: $draft,
                selection: $selection,
                placeholder: "Do anything…",
                sendEnabled: space != nil,
                showStop: false,
                busy: busy,
                alwaysExpanded: true,
                onSend: send,
                attachments: attachments,
                onAttach: { showPhotoPicker = true },
                onRemoveAttachment: { id in attachments.removeAll { $0.id == id } },
                onFocusChange: { composerFocused = $0 }
            ) {
                // Model + trait chips, split like the desktop's footer pickers
                // (they ride right of the shell's attach button).
                ComposerChip(label: selectedModel.label, badgeHarness: harness) {
                    dismissKeyboard()
                    showPicker = true
                }
                if let reasoning {
                    ComposerChip(label: HarnessCatalog.reasoningLabel(reasoning)) {
                        dismissKeyboard()
                        showTraitPicker = true
                    }
                }
            }
        }
    }

    /// Resign the editor's first responder from outside it. ComposerShell owns
    /// its own `@FocusState` and exposes no binding for it, so a local
    /// `@FocusState` here was never attached to anything: the canvas tap and
    /// the picker chips both set it and neither ever put the keyboard down.
    /// Going through the responder chain does, and ComposerShell reports the
    /// resulting blur back through `onFocusChange`, which also closes the
    /// suggestion popover — the same gesture the desktop closes it on.
    private func dismissKeyboard() {
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder),
                                        to: nil, from: nil, for: nil)
    }

    // MARK: Suggestions

    /// Popover visibility: an open trigger AND focus, the live chat's gate. Two
    /// rows read it — the popover itself, and the canvas that gives up its
    /// space for it — so they can never disagree about whether it is up.
    private var suggestionsOpen: Bool {
        draft.trigger(at: selection) != nil && composerFocused
    }

    /// Everything the fetchers need, for a draft that has no chat yet. The
    /// space fixes both the device and the folder, and the file search goes out
    /// as a `spaceId` search: the engine needs exactly one of `chatId` or
    /// `spaceId` (crates/engine/src/rpc.rs), and there is no chat to name.
    ///
    /// `harness` is read here, not captured, so flipping the agent chip
    /// mid-draft re-keys the command cache and fetches the new agent's list.
    private var suggestionContext: SuggestionContext {
        SuggestionContext(harness: harness,
                          deviceId: space?.deviceId ?? "",
                          cwd: slashCwd(chatCwd: nil, spacePath: space?.path),
                          chatId: nil,
                          spaceId: space?.id)
    }

    private func pick(_ item: SuggestionItem, over range: Range<Int>) {
        switch item.payload {
        case .command(let name):
            draft.apply(command: name, over: range, selection: &selection)
        case .path(let path, let isDir):
            draft.apply(path: path, isDir: isDir, over: range, selection: &selection)
        }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    private func refreshSuggestions() async {
        // No space means no device and no folder to ask. Reset rather than
        // return: `scheduleRefresh` has already marked the fetch pending, and
        // nothing else would ever clear that spinner.
        guard space != nil, let trigger = draft.trigger(at: selection) else {
            suggestions.reset()
            return
        }
        await suggestions.update(trigger: trigger, context: suggestionContext)
    }

    /// Debounce and cancel — ComposerView.scheduleRefresh's twin, and for the
    /// same reasons: one keystroke changes BOTH `draft` and `selection`, and
    /// the trigger check has to run synchronously on this keystroke so a closed
    /// trigger drops its stale rows and an open one shows its pending state
    /// through the debounce window.
    private func scheduleRefresh() {
        refreshTask?.cancel()
        guard draft.trigger(at: selection) != nil else {
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

    /// Load picked photos into staged attachments (ComposerView.stage's twin —
    /// the upload happens on send, once the chat and its store exist).
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
            attachError = failed > 0
                ? (failed == 1 ? "One image couldn't be attached (unsupported or over 24 MB)."
                               : "\(failed) images couldn't be attached (unsupported or over 24 MB).")
                : nil
        }
    }

    private func chip(icon: LineIcon, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 6) {
                LineIconView(icon, size: 13, color: Theme.textMuted)
                Text(label)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text.opacity(0.9))
                    .lineLimit(1)
            }
            .padding(.horizontal, 12)
            .frame(height: 40)
            .background(whiteAlpha(0.08), in: Capsule())
            .overlay(Capsule().strokeBorder(whiteAlpha(0.08), lineWidth: 1))
        }
        .buttonStyle(ChipPressButtonStyle())
    }

    // MARK: Checkout model (pickers.rs port)

    private var selectedRefRow: RepoRef? {
        refs.first { $0.name == selectedRef }
    }

    /// checkout_label: New worktree / Current worktree / Current checkout.
    private var checkoutLabel: String {
        switch checkoutKind {
        case .newWorktree: return "New worktree"
        case .local: return selectedRefRow?.worktreePath != nil ? "Current worktree" : "Current checkout"
        }
    }

    private var checkoutIcon: LineIcon {
        checkoutKind == .local && selectedRefRow?.worktreePath == nil ? .folder : .folderWithFiles
    }

    /// ref_label: "From <ref>" only when a NEW worktree will be created off it.
    private var refLabel: String {
        guard let name = selectedRef else { return "Select ref" }
        return checkoutKind == .newWorktree ? "From \(name)" : name
    }

    /// pick_ref (draft mode): a worktree'd ref flips to "Current worktree";
    /// base picks just record; a plain non-current ref in Local mode CHECKS
    /// OUT the space folder (it must never silently flip the mode).
    private func pickRef(_ row: RepoRef) async -> String? {
        if row.worktreePath != nil {
            selectedRef = row.name
            checkoutKind = .local
            return nil
        }
        if checkoutKind == .newWorktree || row.current {
            selectedRef = row.name
            return nil
        }
        guard let space else { return nil }
        let error = await model.switchSpaceRef(space: space, refName: row.name)
        if error == nil {
            selectedRef = row.name
            if let reloaded = await model.listRefs(space: space) {
                refs = reloaded
            }
        }
        return error
    }

    /// pick_checkout: dropping back to Local with a plain non-current ref
    /// picked drops the pick — the current branch takes over.
    private func pickCheckout(_ kind: CheckoutKind) {
        if kind == .local, checkoutKind == .newWorktree,
           let row = selectedRefRow, row.worktreePath == nil, !row.current {
            selectedRef = refs.first(where: \.current)?.name
        }
        checkoutKind = kind
    }

    private var canSend: Bool {
        guard !busy, space != nil else { return false }
        return !draft.plainText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || !attachments.isEmpty
    }

    private func offlineNotice(space: Space) -> some View {
        Text("\(model.deviceName(space.deviceId)) is offline — the run will start when it reconnects.")
            .font(Theme.sans(12))
            .foregroundStyle(Theme.warning.opacity(0.9))
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(Theme.warning.opacity(0.1), in: RoundedRectangle(cornerRadius: 12))
            .padding(.horizontal, 12)
            .padding(.bottom, 8)
    }

    /// Mint the chat per the checkout plan, queue the first run, swap to the
    /// live session (composer.rs on-send: current checkout as-is, reuse the
    /// picked ref's worktree, or CreateWorktree off the base first).
    private func send() {
        guard let space, canSend else { return }
        let prompt = draft.markdown().trimmingCharacters(in: .whitespacesAndNewlines)
        busy = true
        let config = ChatConfig(harness: harness, model: selectedModel.id,
                                reasoning: reasoning, sandbox: "workspace-write")
        Task { @MainActor in
            var cwd: String?
            var branch = selectedRef
            switch checkoutKind {
            case .newWorktree:
                if let base = selectedRef {
                    guard let worktreePath = await model.createWorktree(space: space, base: base) else {
                        busy = false
                        return
                    }
                    cwd = worktreePath
                    branch = base
                }
            case .local:
                if let worktree = selectedRefRow?.worktreePath {
                    cwd = worktree  // reuse the ref's existing checkout
                }
            }
            guard let chatId = model.createChat(space: space, config: config,
                                                branch: branch, cwd: cwd),
                  let chat = model.chat(id: chatId),
                  let store = model.sessionStore(for: chat) else {
                busy = false
                return
            }
            // Upload staged images now that the chat's store exists; the doc
            // entry must never point at files that don't (ComposerView.send).
            var paths: [String] = []
            for att in attachments {
                do {
                    let path = try await store.uploadAttachment(name: att.name, data: att.data)
                    AttachmentImageCache.shared.seed(deviceId: chat.deviceId, path: path,
                                                     name: att.name, data: att.data)
                    paths.append(path)
                } catch {
                    attachError = "Attachment upload failed — \(error.localizedDescription)"
                    busy = false
                    return
                }
            }
            store.sendRun(prompt: paths.isEmpty ? prompt : withAttachments(text: prompt, paths: paths),
                          chat: chat, attachments: paths)
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            draft.clear(selection: &selection)
            attachments = []
            busy = false
            // Replace the canvas with the live session (in-place swap, no
            // back-through-canvas).
            if path.last == .newSession(spaceId: spaceId) {
                path.removeLast()
            }
            path.append(.chat(chatId))
        }
    }
}

// MARK: - Composer chip

/// The composer's picker trigger chip: optional brand mark, label, chevron —
/// one per picker, split like the desktop's footer (model | traits).
struct ComposerChip: View {
    let label: String
    var badgeHarness: String?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                if let badgeHarness {
                    HarnessBadge(harness: badgeHarness, size: 15)
                }
                Text(label)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text.opacity(0.9))
                    .lineLimit(1)
                Image(systemName: "chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
            .padding(.horizontal, 13)
            .frame(height: 40)
            .background(whiteAlpha(0.08), in: Capsule())
            .overlay(Capsule().strokeBorder(whiteAlpha(0.08), lineWidth: 1))
        }
        .buttonStyle(ChipPressButtonStyle())
    }
}

// MARK: - Model picker sheet

/// Detent bottom sheet in the t3 settings-sheet layout: one scrolling list of
/// models sectioned per harness (collapsible uppercase provider headers with
/// the brand mark — picking a model picks its harness), the selected row a
/// filled high-contrast pill with a trailing checkmark. Effort lives in its
/// own TraitPickerSheet, split like the desktop's footer pickers. Harness
/// sections collapse to one once a chat exists — harness is locked mid-chat.
struct ModelPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Binding var harness: String
    @Binding var modelId: String
    @Binding var reasoning: String?
    /// True when reconfiguring a live chat: the harness can't change mid-chat.
    var lockedHarness = false
    /// Harness sections to offer (the device's live list; static fallback).
    var harnesses: [HarnessInfo] = []
    /// Live per-harness catalogs from the device (static fallback when absent).
    var catalogs: [String: [ModelInfo]] = [:]

    private func models(for harness: String) -> [ModelInfo] {
        catalogs[harness] ?? HarnessCatalog.models(for: harness)
    }

    private var sections: [HarnessInfo] {
        if lockedHarness {
            return [HarnessInfo(id: harness, label: HarnessCatalog.label(for: harness))]
        }
        return harnesses.isEmpty ? HarnessCatalog.harnesses : harnesses
    }

    /// Accordion state: which harness sections show their models. Seeded with
    /// the current harness — with several agents enabled a flat list of every
    /// catalog is unmanageable (t3's collapsible provider folds).
    @State private var openSections: Set<String> = []

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    VStack(alignment: .leading, spacing: 4) {
                        SheetLabel("Model")
                        ForEach(sections) { h in
                            if sections.count > 1 {
                                sectionHeader(h)
                            }
                            if sections.count == 1 || openSections.contains(h.id) {
                                ForEach(models(for: h.id)) { m in
                                    PickRow(title: m.label,
                                            subtitle: m.description,
                                            selected: harness == h.id && m.id == modelId) {
                                        select(harness: h.id, model: m)
                                    }
                                }
                            }
                        }
                    }
                    .onAppear { openSections = [harness] }
                }
                .padding(20)
                .padding(.bottom, 12)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Select model")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    private var selectedModel: ModelInfo? {
        models(for: harness).first { $0.id == modelId }
    }

    /// t3's collapsible ProviderHeader: brand mark + tracked-out uppercase
    /// provider name, trailing model count + chevron; tapping folds the section.
    private func sectionHeader(_ h: HarnessInfo) -> some View {
        let open = openSections.contains(h.id)
        return Button {
            UISelectionFeedbackGenerator().selectionChanged()
            withAnimation(Motion.collapse) {
                if open { openSections.remove(h.id) } else { openSections.insert(h.id) }
            }
        } label: {
            HStack(spacing: 7) {
                HarnessBadge(harness: h.id, size: 13)
                Text(h.label.uppercased())
                    .font(Theme.sans(10.5, weight: .medium))
                    .kerning(1.2)
                    .foregroundStyle(Theme.textMuted.opacity(0.7))
                Spacer(minLength: 8)
                if !open {
                    Text("\(models(for: h.id).count)")
                        .font(Theme.sans(10.5))
                        .foregroundStyle(Theme.textFaint)
                }
                Image(systemName: open ? "chevron.up" : "chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
            .padding(.horizontal, 4)
            .padding(.top, 12)
            .padding(.bottom, 6)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func select(harness harnessId: String, model m: ModelInfo) {
        UISelectionFeedbackGenerator().selectionChanged()
        if harness != harnessId {
            harness = harnessId
        }
        modelId = m.id
        if let current = reasoning, m.reasoningLevels.contains(current) {
            return
        }
        reasoning = HarnessCatalog.defaultReasoning(for: m)
    }

}

// MARK: - Trait (effort) picker sheet

/// The effort ladder in its own detent sheet — the composer's second picker
/// chip, split from the model list like the desktop's Traits dropdown.
struct TraitPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Binding var reasoning: String?
    let levels: [String]

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 4) {
                    SheetLabel("Effort")
                    ForEach(levels, id: \.self) { level in
                        PickRow(title: HarnessCatalog.reasoningLabel(level),
                                subtitle: Self.effortHint(level),
                                selected: reasoning == level) {
                            UISelectionFeedbackGenerator().selectionChanged()
                            reasoning = level
                        }
                    }
                }
                .padding(20)
                .padding(.bottom, 12)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Traits")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    /// One-line hints for the ladder (the special modes deserve explanation).
    static func effortHint(_ level: String) -> String? {
        switch level {
        case "minimal": return "Quickest, lightest touch"
        case "low": return "Fastest responses"
        case "medium": return "Balanced speed and depth"
        case "high": return "Thorough reasoning"
        case "xhigh": return "Extended reasoning"
        case "max": return "Maximum reasoning budget"
        case "ultra": return "Highest Codex tier"
        case "ultracode": return "X-High plus the ultracode setting"
        case "ultrathink": return "Deep-thinking prompt mode"
        default: return nil
        }
    }
}

// MARK: - Shared picker row

/// t3's ModelRow — the ONE row style every picker sheet uses (model, trait,
/// ref, checkout): the selected row is a filled high-contrast pill with a
/// trailing checkmark; unselected rows sit almost flat on the sheet. Optional
/// leading line icon and a busy spinner for rows whose pick runs async (git
/// checkouts).
struct PickRow: View {
    let title: String
    var subtitle: String?
    var icon: LineIcon?
    var busy = false
    let selected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 10) {
                if let icon {
                    LineIconView(icon, size: 15,
                                 color: selected ? Theme.bg : Theme.textMuted)
                        .frame(width: 20)
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(Theme.sans(15, weight: .medium))
                        .foregroundStyle(selected ? Theme.bg : Theme.text)
                    if let subtitle {
                        Text(subtitle)
                            .font(Theme.sans(12))
                            .foregroundStyle(selected ? Theme.bg.opacity(0.65) : Theme.textMuted)
                    }
                }
                Spacer(minLength: 8)
                if busy {
                    ProgressView()
                        .controlSize(.small)
                        .tint(selected ? Theme.bg : Theme.textMuted)
                } else {
                    Image(systemName: "checkmark")
                        .font(.system(size: 13, weight: .semibold))
                        .foregroundStyle(Theme.bg)
                        .opacity(selected ? 1 : 0)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 11)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(selected ? AnyShapeStyle(Theme.text) : AnyShapeStyle(whiteAlpha(0.03)),
                        in: RoundedRectangle(cornerRadius: 12))
            .contentShape(RoundedRectangle(cornerRadius: 12))
        }
        .buttonStyle(SheetRowButtonStyle())
    }
}

// MARK: - Ref picker sheet

/// Base-ref selector (the desktop footer's branch popover): branch rows with
/// current-checkout / worktree markers. Picks that require a git checkout run
/// inline — a spinner on the row, git's error surfaced in place on failure
/// (dirty tree, held ref), success dismisses.
struct RefPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let refs: [RepoRef]
    let selected: String?
    /// Returns an error message to keep the sheet open, or nil to close.
    let onPick: (RepoRef) async -> String?

    @State private var switching: String?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 4) {
                    SheetLabel("Ref")
                    if refs.isEmpty {
                        Text("Loading refs from the device…")
                            .font(Theme.sans(13))
                            .foregroundStyle(Theme.textFaint)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 28)
                    } else {
                        ForEach(refs, id: \.name) { ref in
                            row(ref)
                        }
                    }
                    if let error {
                        Text(error)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.danger)
                            .padding(.horizontal, 4)
                            .padding(.top, 4)
                    }
                }
                .padding(20)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Select ref")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    private func row(_ ref: RepoRef) -> some View {
        PickRow(title: ref.name,
                subtitle: subtitle(for: ref),
                busy: switching == ref.name,
                selected: ref.name == selected) {
            guard switching == nil else { return }
            UISelectionFeedbackGenerator().selectionChanged()
            error = nil
            switching = ref.name
            Task { @MainActor in
                let result = await onPick(ref)
                switching = nil
                if let result {
                    error = result
                } else {
                    dismiss()
                }
            }
        }
    }

    private func subtitle(for ref: RepoRef) -> String? {
        if ref.current { return "Current checkout" }
        if ref.worktreePath != nil { return "Checked out in a worktree" }
        return nil
    }
}

// MARK: - Checkout picker sheet

/// Where the session runs (the desktop's checkout popover): the space's
/// folder as-is (or the picked ref's existing worktree), or a fresh isolated
/// worktree created off the base ref on send.
struct CheckoutPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let kind: CheckoutKind
    let selectedRefHasWorktree: Bool
    let onPick: (CheckoutKind) -> Void

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 4) {
                    SheetLabel("Checkout")
                    row(.local,
                        title: selectedRefHasWorktree ? "Current worktree" : "Current checkout",
                        subtitle: selectedRefHasWorktree
                            ? "Reuse the picked ref's existing worktree"
                            : "Run in the space's folder as-is")
                    row(.newWorktree, title: "New worktree",
                        subtitle: "A fresh isolated worktree created off the picked base ref")
                }
                .padding(20)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Checkout")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    private func row(_ rowKind: CheckoutKind, title: String, subtitle: String) -> some View {
        PickRow(title: title, subtitle: subtitle, selected: rowKind == kind) {
            UISelectionFeedbackGenerator().selectionChanged()
            onPick(rowKind)
            dismiss()
        }
    }
}
