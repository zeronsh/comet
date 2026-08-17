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
            ForEach(Array(queue.enumerated()), id: \.element.id) { index, item in
                row(item, index: index, count: queue.count)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 16)
        .motionAnimation(Motion.resize, value: queue.map(\.id))
    }

    private func row(_ item: QueuedMessage, index: Int, count: Int) -> some View {
        let editing = editingId == item.id
        return HStack(spacing: 8) {
            Text("\(index + 1)")
                .font(Theme.mono(10))
                .foregroundStyle(Theme.textFaint)
                .frame(minWidth: 12, alignment: .trailing)
            Text(editing ? "Editing below" : MessageQueue.oneLine(item.text))
                .font(Theme.sans(12.5))
                .foregroundStyle(editing ? Theme.textMuted : Theme.text)
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
            if !item.attachments.isEmpty {
                Image(systemName: "paperclip")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
            controls(item, index: index, count: count, editing: editing)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .background(whiteAlpha(dragging == item.id ? 0.10 : 0.04),
                    in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Theme.border, lineWidth: 1))
        .contentShape(RoundedRectangle(cornerRadius: 10))
        // Drag to reorder, with the arrows below doing the same thing for
        // anyone who would rather not hold a row steady on a moving list.
        .draggable(item.id) {
            Text(MessageQueue.oneLine(item.text))
                .font(Theme.sans(12.5))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
                .padding(8)
        }
        .dropDestination(for: String.self) { ids, _ in
            guard let dropped = ids.first, dropped != item.id else { return false }
            store.moveQueued(id: dropped, to: index)
            return true
        } isTargeted: { targeted in
            dragging = targeted ? item.id : (dragging == item.id ? nil : dragging)
        }
    }

    private func controls(_ item: QueuedMessage, index: Int, count: Int,
                          editing: Bool) -> some View {
        HStack(spacing: 2) {
            iconButton("chevron.up", label: "Move up", enabled: index > 0) {
                store.moveQueued(id: item.id, by: -1)
            }
            iconButton("chevron.down", label: "Move down", enabled: index < count - 1) {
                store.moveQueued(id: item.id, by: 1)
            }
            iconButton(editing ? "xmark" : "pencil",
                       label: editing ? "Stop editing" : "Edit") {
                if editing {
                    onCancelEdit()
                } else {
                    onEdit(item)
                }
            }
            // Sending one now stops whatever the agent is doing to take it —
            // the row leaves the queue as a turn, not as a deletion.
            iconButton("arrow.up.circle", label: "Send now") {
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                Task { await store.sendQueuedNow(id: item.id) }
            }
            iconButton("trash", label: "Remove", tone: Theme.textFaint) {
                if editing { onCancelEdit() }
                store.removeQueued(id: item.id)
            }
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
                .frame(width: 26, height: 26)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(!enabled)
        .accessibilityLabel(label)
    }
}
