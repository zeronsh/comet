// The pending-message queue on a session doc (crates/doc/src/queue.rs).
//
// Anything typed mid-turn waits here — on the doc, so the Mac shows the same
// queue this phone does and either can reorder it. The host is the only one
// that takes from it; every device may add, retype, move and drop rows.

import Foundation

/// One unsent message waiting its turn.
struct QueuedMessage: Identifiable, Equatable, Sendable {
    let id: String
    /// What the user typed. Never empty — emptying it deletes the row.
    var text: String
    /// Committed upload paths, staged when the row was queued.
    var attachments: [String] = []
    /// Device that queued it.
    var issuedBy: String = ""
    /// Epoch millis.
    var issuedAt: Int64 = 0
    /// Epoch millis of the last text edit, when there has been one.
    var editedAt: Int64?
}

enum MessageQueue {
    /// The panel header's aside. Nil when nothing waits.
    static func label(_ count: Int) -> String? {
        switch count {
        case ..<1: return nil
        case 1: return "1 queued"
        default: return "\(count) queued"
        }
    }

    /// One line of a queued message: the newlines that make it a paragraph in
    /// the composer make it three rows here, and a row is one line tall.
    static func oneLine(_ text: String) -> String {
        text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
    }

    /// Where the row at `from` lands when moved one slot in `direction`
    /// (-1 up, +1 down), or nil when it is already at that end.
    static func neighbour(of from: Int, direction: Int, count: Int) -> Int? {
        let to = from + direction
        guard from >= 0, from < count, to >= 0, to < count else { return nil }
        return to
    }
}
