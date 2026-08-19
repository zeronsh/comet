import SwiftUI

enum PullRequestBadgeSurface {
    case sessionRow
    case composer
}

extension ChangeRequestState {
    var badgeColor: Color {
        switch self {
        case .open: return Theme.statusCompleted
        case .merged: return Theme.inlineCodeText
        case .closed: return Theme.danger
        }
    }
}

extension ChangeRequestSummary {
    var accessibilityLabel: String {
        "Pull request \(number), \(state.label), \(title)"
    }
}

/// Shared provider-neutral PR affordance. The full title stays available to
/// VoiceOver while narrow surfaces render only the stable numeric identity.
struct PullRequestBadge: View {
    @Environment(\.openURL) private var openURL
    let summary: ChangeRequestSummary
    var surface: PullRequestBadgeSurface = .sessionRow

    private var composer: Bool { surface == .composer }

    var body: some View {
        Button {
            guard let url = URL(string: summary.url) else { return }
            openURL(url)
        } label: {
            HStack(spacing: composer ? 6 : 0) {
                if composer {
                    LineIconView(.pullRequest, size: 14, color: summary.state.badgeColor.opacity(0.9))
                }
                Text("#\(summary.number)")
                    .font(Theme.mono(composer ? 12 : 10, weight: .medium))
                    .lineLimit(1)
            }
            .foregroundStyle(summary.state.badgeColor.opacity(0.9))
            .padding(.horizontal, composer ? 12 : 5)
            .frame(height: composer ? 40 : 18)
            .background(summary.state.badgeColor.opacity(0.09), in: badgeShape)
            .overlay(badgeShape.strokeBorder(summary.state.badgeColor.opacity(0.13), lineWidth: 1))
            .contentShape(badgeShape)
        }
        .buttonStyle(.plain)
        .accessibilityLabel(summary.accessibilityLabel)
        .accessibilityHint("Opens in your browser")
    }

    private var badgeShape: RoundedRectangle {
        RoundedRectangle(cornerRadius: composer ? 12 : 5)
    }
}

/// Read-only checkout context in an existing session's composer.
struct BranchContextChip: View {
    let branch: String

    var body: some View {
        HStack(spacing: 6) {
            LineIconView(.gitBranch, size: 14, color: Theme.textMuted)
            Text(branch)
                .font(Theme.sans(12, weight: .medium))
                .foregroundStyle(Theme.textMuted)
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .padding(.horizontal, 12)
        .frame(height: 40)
        .background(whiteAlpha(0.06), in: Capsule())
        .overlay(Capsule().strokeBorder(whiteAlpha(0.08), lineWidth: 1))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Branch \(branch)")
    }
}
