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
    /// capping at it makes the panel hug one row and still stop at 180.
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
                // maxHeight, NOT height. An exact height cannot compress, so on
                // a page whose other rows already fill the space above the
                // keyboard the panel kept its full 180pt and the whole column
                // overflowed instead: the canvas behind it was squeezed under
                // its own content's minimum and drew over the navigation bar,
                // and the pill's chip row went under the keyboard (user report,
                // new-session page on a real device). A maximum is the same
                // hug — the ScrollView is greedy up to it — but it yields when
                // the parent has less to give.
                .frame(maxHeight: min(contentHeight, Self.maxListHeight))
                .scrollDisabled(contentHeight <= Self.maxListHeight)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        // Clip to the same shape the glass draws, so a row the panel had to
        // compress away cannot bleed past the rounded bottom edge.
        .clipShape(surfaceShape)
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
