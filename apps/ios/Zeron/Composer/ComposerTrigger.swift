// Composer trigger detection: a port of the desktop composer's slash_token
// and mention_token (crates/ui/src/composer.rs:3179-3233). Pure — no SwiftUI,
// no attributed text, no I/O — so it is testable without a simulator and it
// stays correct if the rich-text layer above it is ever replaced.
//
// All offsets count Characters. Rust counts UTF-8 bytes; do not mix them.

import Foundation

enum TriggerKind {
    case command
    case path
}

struct Trigger: Equatable {
    let kind: TriggerKind
    /// Text between the sigil and the caret. What the popover filters on.
    let query: String
    /// The FULL text of `range`, including the sigil and anything after the
    /// caret. The dismissal rule keys on this, not on `query`: moving the caret
    /// inside a dismissed token must keep it closed, while any edit reopens it
    /// (crates/ui/src/composer.rs:3302-3304). `query` changes on a caret move;
    /// `token` does not.
    let token: String
    /// The span the pick replaces, including the sigil. For a path this
    /// extends past the caret to the end of the token.
    let range: Range<Int>
}

enum ComposerTrigger {

    static func detect(text: String, caret: Int) -> Trigger? {
        let chars = Array(text)
        guard caret >= 0, caret <= chars.count else { return nil }
        return command(chars, caret) ?? path(chars, caret)
    }

    static func replace(_ text: String, range: Range<Int>,
                        with replacement: String) -> (text: String, caret: Int) {
        let chars = Array(text)
        let lower = max(0, min(chars.count, range.lowerBound))
        let upper = max(lower, min(chars.count, range.upperBound))
        let next = String(chars[0..<lower]) + replacement + String(chars[upper...])
        return (next, lower + replacement.count)
    }

    // MARK: Command

    /// The `/` must open the whole draft: slash commands are whole-prompt
    /// prefixes (`/compact`, `/goal ship it`), so only the first token
    /// triggers, and a query holding another `/` (a typed path) never does.
    private static func command(_ chars: [Character], _ caret: Int) -> Trigger? {
        guard chars.first == "/" else { return nil }
        let end = chars.firstIndex(where: \.isWhitespace) ?? chars.count
        guard caret > 0, caret <= end else { return nil }
        let query = String(chars[1..<caret])
        guard !query.contains("/") else { return nil }
        return Trigger(kind: .command, query: query,
                       token: String(chars[0..<end]), range: 0..<end)
    }

    // MARK: Path

    /// The `@` must begin a token. This excludes `name@example.com` and
    /// ordinary words while allowing punctuation such as `(@src`.
    private static func path(_ chars: [Character], _ caret: Int) -> Trigger? {
        var tokenStart = 0
        var back = caret - 1
        while back >= 0 {
            if chars[back].isWhitespace {
                tokenStart = back + 1
                break
            }
            back -= 1
        }

        var at: Int?
        var scan = caret - 1
        while scan >= tokenStart {
            if chars[scan] == "@" {
                at = scan
                break
            }
            scan -= 1
        }
        guard let at else { return nil }

        let validBoundary: Bool
        if at == 0 {
            validBoundary = true
        } else {
            let previous = chars[at - 1]
            validBoundary = previous.isWhitespace
                || previous == "(" || previous == "[" || previous == "{"
        }
        guard validBoundary else { return nil }

        let query = String(chars[(at + 1)..<caret])
        guard !query.contains("@") else { return nil }

        // The token ends at the first whitespace AT OR AFTER the caret, so
        // editing the middle of a mention replaces the whole token.
        var end = chars.count
        var forward = caret
        while forward < chars.count {
            if chars[forward].isWhitespace {
                end = forward
                break
            }
            forward += 1
        }

        return Trigger(kind: .path, query: query,
                       token: String(chars[at..<end]), range: at..<end)
    }
}
