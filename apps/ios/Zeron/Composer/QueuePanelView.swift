// Queued messages, stacked directly above the composer.
//
// Everything typed while the agent is busy waits here (crates/doc/src/queue.rs)
// until the host sends it. Rows can be reordered by dragging or with the
// arrows, retyped in the composer, sent immediately (which stops the turn to
// do it), or dropped.

import SwiftUI
import UniformTypeIdentifiers

struct QueuePanel: View {
    let store: SessionStore
    /// The row currently being retyped in the composer, if any.
    var editingId: String?
    var onEdit: (QueuedMessage) -> Void
    var onCancelEdit: () -> Void
    var supportsActions: Bool
    var onAction: (QueuedMessage, QueueAction) -> Void

    @State private var dragging: String?

    var body: some View {
        let queue = store.queue
        VStack(alignment: .leading, spacing: 6) {
            if let label = MessageQueue.label(queue.count) {
                Text(label.uppercased())
                    .font(Theme.sans(10, weight: .medium))
                    .kerning(0.6)
                    .foregroundStyle(Theme.textFaint)
                    .padding(.leading, 4)
            }
            ScrollView {
                LazyVStack(spacing: 6) {
                    ForEach(Array(queue.enumerated()), id: \.element.id) { index, item in
                        row(item, index: index, count: queue.count)
                    }
                }
            }
            .frame(height: CGFloat(min(queue.count, 3)) * 56 + CGFloat(max(0, min(queue.count, 3) - 1)) * 6)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 16)
        .motionAnimation(Motion.resize, value: queue.map(\.id))
    }

    private func row(_ item: QueuedMessage, index: Int, count: Int) -> some View {
        let editing = editingId == item.id
        let displayText: String = {
            if store.queueActionsPending.contains(item.id) { return "Updating…" }
            if editing { return "Editing below" }
            switch item.deliveryGate {
            case .editing(let owner, _): return "Editing on \(owner)"
            case .reviewRequired: return "Needs review"
            case nil:
                return MessageQueue.oneLine(
                    MessageQueue.visibleText(item.text, attachments: item.attachments)
                )
            }
        }()
        let sources = AppshotContext.presentations(item.text)
        let sourceNames = item.attachments.map { sources[$0].map { "\($0.appName) Appshot" } ?? "Image" }
        let content = HStack(spacing: 6) {
            Text("\(index + 1)")
                .font(Theme.mono(10))
                .foregroundStyle(Theme.textFaint)
                .frame(minWidth: 12, alignment: .trailing)
            if let path = item.attachments.first, !editing {
                QueueAttachmentPreview(deviceId: store.hostDeviceId ?? store.deviceId,
                                       path: path, paths: item.attachments)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(displayText)
                    .font(Theme.sans(12.5))
                    .foregroundStyle(editing ? Theme.textMuted : Theme.text)
                    .lineLimit(1)
                if !sourceNames.isEmpty && !editing {
                    Text(sourceNames.joined(separator: " · "))
                        .font(Theme.sans(10.5)).foregroundStyle(Theme.textMuted).lineLimit(1)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            controls(item, index: index, count: count, editing: editing)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(whiteAlpha(dragging == item.id ? 0.10 : 0.04),
                    in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Theme.border, lineWidth: 1))
        .contentShape(RoundedRectangle(cornerRadius: 10))
        return draggable(content, item: item)
        // Drag to reorder, with the arrows below doing the same thing for
        // anyone who would rather not hold a row steady on a moving list.
        .dropDestination(for: String.self) { ids, _ in
            guard let dropped = ids.first, dropped != item.id else { return false }
            store.moveQueued(id: dropped, to: index)
            return true
        } isTargeted: { targeted in
            dragging = targeted ? item.id : (dragging == item.id ? nil : dragging)
        }
    }

    /// Keep a protected row stationary while its text is being edited or
    /// awaiting review. Other rows may still use it as a drop destination.
    @ViewBuilder
    private func draggable<Content: View>(_ content: Content,
                                         item: QueuedMessage) -> some View {
        if item.deliveryGate == nil && !store.queueActionsPending.contains(item.id) {
            content.draggable(item.id) {
                Text(MessageQueue.oneLine(
                    MessageQueue.visibleText(item.text, attachments: item.attachments)
                ))
                    .font(Theme.sans(12.5))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                    .padding(8)
            }
        } else {
            content
        }
    }

    private func controls(_ item: QueuedMessage, index: Int, count: Int,
                          editing: Bool) -> some View {
        let pending = store.queueActionsPending.contains(item.id)
        let gated = item.deliveryGate != nil || pending
        let primary = MessageQueue.primaryAction(for: item,
                                                 supportsActions: supportsActions, pending: pending)
        let lockedByOther: Bool = {
            if case .editing = item.deliveryGate { return !editing }
            return false
        }()
        return HStack(spacing: 0) {
            iconButton(editing ? "xmark" : "pencil",
                       label: editing ? "Stop editing" : "Edit",
                       enabled: !lockedByOther && !pending) {
                if editing { onCancelEdit() } else { onEdit(item) }
            }
            iconButton("arrow.right", label: "Send now, interrupting the response",
                       enabled: primary != nil) {
                if let primary { onAction(item, primary) }
            }
            Menu {
                Button("Move up", systemImage: "chevron.up") { store.moveQueued(id: item.id, by: -1) }
                    .disabled(index == 0 || gated)
                Button("Move down", systemImage: "chevron.down") { store.moveQueued(id: item.id, by: 1) }
                    .disabled(index >= count - 1 || gated)
                Button("Remove", systemImage: "trash", role: .destructive) { onAction(item, .remove) }
                    .disabled(!supportsActions || pending)
            } label: {
                Image(systemName: "ellipsis").font(.system(size: 13))
                    .foregroundStyle(Theme.textMuted).frame(width: 44, height: 44)
                    .contentShape(Rectangle())
            }
            .accessibilityLabel("More queue actions")
        }
    }

    private func iconButton(
        _ symbol: String,
        label: String,
        enabled: Bool = true,
        tone: Color = Theme.textMuted,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(enabled ? tone : Theme.textFaint.opacity(0.4))
                .frame(width: 44, height: 44)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(!enabled)
        .accessibilityLabel(label)
    }
}

/// The queue holds a tiny decoded preview; opening it uses the shared full
/// image. Its task identity is stable across row updates and cache eviction.
private struct QueueAttachmentPreview: View {
    let deviceId: String
    let path: String
    let paths: [String]
    private var extraCount: Int { paths.count - 1 }
    private let cache = AttachmentImageCache.shared
    @State private var preview: AttachmentPreview?
    @State private var thumbnail: UIImage?
    @State private var gallery = false

    private var sourceImage: UIImage? {
        if case .loaded(_, let image) = cache.snapshot(deviceId: deviceId, path: path) { return image }
        return nil
    }

    var body: some View {
        Button {
            if extraCount > 0 { gallery = true; return }
            if case .loaded(let name, let image) = cache.snapshot(deviceId: deviceId, path: path) {
                preview = AttachmentPreview(name: name, image: image)
            } else { cache.load(deviceId: deviceId, path: path) }
        } label: {
            ZStack(alignment: .bottomTrailing) {
                Group {
                    if let thumbnail {
                        Image(uiImage: thumbnail).resizable().scaledToFill()
                    } else { Image(systemName: "photo").foregroundStyle(Theme.textFaint) }
                }
                .frame(width: 40, height: 28).clipped()
                if extraCount > 0 {
                    Text("+\(extraCount)").font(Theme.sans(9, weight: .medium))
                        .padding(.horizontal, 3).background(Theme.bg.opacity(0.9), in: RoundedRectangle(cornerRadius: 3))
                        .foregroundStyle(Theme.text)
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: 5))
            .frame(width: 40, height: 44).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(extraCount > 0 ? "Preview image, \(extraCount) more attachments" : "Preview image")
        .task(id: "\(deviceId)|\(path)") { cache.load(deviceId: deviceId, path: path) }
        .task(id: sourceImage) {
            if let image = sourceImage, let small = await image.byPreparingThumbnail(ofSize: CGSize(width: 80, height: 56)) {
                thumbnail = small
            }
        }
        .fullScreenCover(item: $preview) { AttachmentLightbox(preview: $0) }
        .sheet(isPresented: $gallery) {
            NavigationStack {
                ScrollView(.horizontal) {
                    LazyHStack(spacing: 12) {
                        ForEach(paths, id: \.self) { path in
                            VStack(spacing: 8) {
                                AttachmentThumbView(deviceId: deviceId, path: path)
                                if case .loaded(let name, _) = cache.snapshot(deviceId: deviceId, path: path) {
                                    Text(name).font(Theme.sans(11)).foregroundStyle(Theme.textMuted).lineLimit(1)
                                }
                            }.frame(width: 112)
                        }
                    }.padding(20)
                }
                .navigationTitle("Queued images")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { gallery = false } } }
            }.presentationDetents([.height(240), .medium])
        }
    }
}
