// The suggestion list that floats above the composer pill. Presentation only:
// it takes values and hands back a pick. Matching the app's glass language, it
// reuses Theme and the same rounded-surface treatment as ComposerShell.

import SwiftUI

struct ComposerPopover: View {

    let items: [SuggestionItem]
    let kind: TriggerKind
    let isLoading: Bool
    let errorText: String?
    let onPick: (SuggestionItem) -> Void

    /// Height of the rows, measured. A `ScrollView` is greedy: it takes every
    /// point offered up to its `maxHeight`, so a `.frame(maxHeight: 180)` alone
    /// left a single result floating in a 180pt panel. Measuring the content and
    /// setting an exact height makes the panel hug one row and still cap at 180.
    @State private var contentHeight: CGFloat = 0

    private var surfaceShape: RoundedRectangle { RoundedRectangle(cornerRadius: 20) }

    private static let maxListHeight: CGFloat = 180

    private var header: String {
        switch kind {
        case .command: "Commands"
        case .path: "Files"
        }
    }

    private var emptyText: String {
        if let errorText { return errorText }
        if isLoading { return kind == .path ? "Searching files…" : "Loading…" }
        switch kind {
        case .command: return "No matching commands."
        case .path: return "No matching files or folders."
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(header.uppercased())
                .font(Theme.sans(10, weight: .bold))
                .kerning(0.8)
                .foregroundStyle(Theme.textMuted)
                .padding(.horizontal, 14)
                .padding(.top, 10)
                .padding(.bottom, 4)

            if items.isEmpty {
                Text(emptyText)
                    .font(Theme.sans(12))
                    .foregroundStyle(errorText == nil ? Theme.textFaint : Theme.danger)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 8)
            } else {
                ScrollView {
                    VStack(spacing: 0) {
                        ForEach(Array(items.enumerated()), id: \.element.id) { index, item in
                            row(item, isLast: index == items.count - 1)
                        }
                    }
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height in
                        contentHeight = height
                    }
                }
                .frame(height: min(contentHeight, Self.maxListHeight))
                .scrollDisabled(contentHeight <= Self.maxListHeight)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(whiteAlpha(0.04), in: surfaceShape)
        .glassEffect(.regular, in: surfaceShape)
        .overlay(surfaceShape.strokeBorder(whiteAlpha(0.05), lineWidth: 1))
    }

    private func row(_ item: SuggestionItem, isLast: Bool) -> some View {
        Button {
            onPick(item)
        } label: {
            HStack(spacing: 10) {
                Image(systemName: icon(for: item))
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.textMuted)
                    .frame(width: 16)
                Text(item.label)
                    .font(Theme.sans(15, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                if !item.detail.isEmpty {
                    Text(item.detail)
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .contentShape(Rectangle())
            .overlay(alignment: .bottom) {
                if !isLast {
                    Rectangle().fill(whiteAlpha(0.06)).frame(height: 0.5)
                }
            }
        }
        .buttonStyle(.plain)
    }

    private func icon(for item: SuggestionItem) -> String {
        switch item.payload {
        case .command: "terminal"
        case .path(_, let isDir): isDir ? "folder" : "doc"
        }
    }
}
