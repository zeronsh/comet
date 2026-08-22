// The composer's text model. This is the ONLY file that knows AttributedString
// exists: ComposerTrigger, ComposerSuggestions, and ComposerPopover all work on
// plain text and a caret offset. If rich text ever has to be abandoned, the
// blast radius is this file.
//
// A mention chip is styled text, not an embedded view — the desktop defines the
// same look (crates/ui/src/composer.rs:643-647). The PATH lives in the
// attribute, never in the visible text, so editing the label cannot silently
// retarget a mention.

import SwiftUI

struct MentionValue: Hashable, Sendable {
    let path: String
    let isDir: Bool
}

enum MentionAttribute: AttributedStringKey {
    typealias Value = MentionValue
    static let name = "zeronMention"
    /// Text typed beside a chip must never join it.
    static let inheritedByAddedText = false
    /// Ask the framework to drop the attribute when the run's own text
    /// changes. `enforceInvariant` re-checks regardless: this is a fast path,
    /// not the contract.
    static let invalidationConditions: Set<AttributedString.AttributeInvalidationCondition>? = [.textChanged]
}

extension AttributeScopes {
    struct ZeronScope: AttributeScope {
        let zeronMention: MentionAttribute
        let swiftUI: AttributeScopes.SwiftUIAttributes
    }

    var zeron: ZeronScope.Type { ZeronScope.self }
}

// MARK: - Chip paint, derived

/// Chip styling is DERIVED from the mention key. It is never stored on the run.
///
/// The Task 0 spike proved both halves of why, and both are load-bearing:
///
///   - Visual attributes inherit FORWARD. Text typed at a chip's trailing edge
///     picks up a *stored* mono font and wash, because
///     `inheritedByAddedText = false` holds back only the custom key.
///   - `invalidationConditions` strips the custom key without stripping stored
///     paint, so a dead mention would keep looking like a live chip.
///
/// Deriving fixes both ends at once: the paint follows the key, and only the
/// key. Store styling on the run and you reintroduce both bugs.
///
/// A `ValueConstraint` may READ any attribute but may WRITE only its own
/// `AttributeKey` (`SwiftUICore.swiftinterface:9535-9547`), so font and
/// background are two constraints rather than one.
struct MentionFormatting: AttributedTextFormattingDefinition {
    typealias Scope = AttributeScopes.ZeronScope

    var body: some AttributedTextFormattingDefinition<Scope> {
        MentionFont()
        MentionWash()
    }
}

struct MentionFont: AttributedTextValueConstraint {
    typealias Scope = AttributeScopes.ZeronScope
    typealias AttributeKey = AttributeScopes.SwiftUIAttributes.FontAttribute

    func constrain(_ container: inout Attributes) {
        container[AttributeKey.self] = container[MentionAttribute.self] == nil
            ? nil
            : .system(size: 15, design: .monospaced)
    }
}

struct MentionWash: AttributedTextValueConstraint {
    typealias Scope = AttributeScopes.ZeronScope
    typealias AttributeKey = AttributeScopes.SwiftUIAttributes.BackgroundColorAttribute

    func constrain(_ container: inout Attributes) {
        container[AttributeKey.self] = container[MentionAttribute.self] == nil
            ? nil
            : Color.white.opacity(0.10)
    }
}

struct ComposerText: Equatable {

    var attributed: AttributedString

    init(_ plain: String = "") {
        attributed = AttributedString(plain)
    }

    // MARK: Plain projection

    var plainText: String { String(attributed.characters) }

    var isEmpty: Bool { attributed.characters.isEmpty }

    // MARK: Selection

    /// The caret offset in Characters, or nil when the selection covers a
    /// range. `.ranges` can be discontiguous, so both cases fall out here.
    func caretOffset(_ selection: AttributedTextSelection) -> Int? {
        switch selection.indices(in: attributed) {
        case .insertionPoint(let index):
            return attributed.characters.distance(from: attributed.startIndex, to: index)
        case .ranges:
            return nil
        }
    }

    /// A trigger, vetoed when it overlaps an existing chip. A chip's visible
    /// text IS `@basename`, so the pure detector cannot tell one from a token
    /// the user typed. Only this file can see the attribute, so the veto lives
    /// here.
    func trigger(at selection: AttributedTextSelection) -> Trigger? {
        guard let caret = caretOffset(selection),
              let candidate = ComposerTrigger.detect(text: plainText, caret: caret),
              !intersectsMention(candidate.range)
        else { return nil }
        return candidate
    }

    private func intersectsMention(_ range: Range<Int>) -> Bool {
        for (lower, upper) in mentionRuns() where range.lowerBound < upper && lower < range.upperBound {
            return true
        }
        return false
    }

    // MARK: Serialization

    /// The draft as the host sees it. Non-mention runs pass through; each
    /// mention run becomes the canonical link built from its ATTRIBUTE.
    func markdown() -> String {
        var out = ""
        // Iterate the MENTION-KEYED run view, never `attributed.runs`. The plain
        // view splits at EVERY attribute boundary, so an unrelated attribute
        // landing on part of a chip — the iOS 26 editor's own formatting menu can
        // underline half a chip, and pasted styled text does it too — breaks one
        // chip into several runs that each carry the same value. This loop would
        // then emit the same link once per fragment. The keyed view coalesces
        // them back into a single slice.
        for (mention, range) in attributed.runs[MentionAttribute.self] {
            if let mention {
                out += MentionLink.serialize(path: mention.path, isDir: mention.isDir)
            } else {
                out += String(attributed[range].characters)
            }
        }
        return out
    }

    // MARK: The invariant

    /// Strip the mention attribute from every run that fails either clause:
    ///
    ///   1. the run's visible text is exactly `@basename`, and
    ///   2. the serialized draft re-parses to that mention at that position.
    ///
    /// Clause 1 keeps the screen and the wire honest — `markdown()` builds the
    /// label from the attribute, so a mangled run would otherwise serialize to
    /// a perfectly valid link for a path the user can no longer see. Clause 2
    /// catches what clause 1 cannot: an unsafe path, an encoding that does not
    /// round-trip, and surrounding text that swallows the link (the desktop
    /// parser reads the WHOLE draft — crates/ui/src/composer.rs:752-795).
    mutating func enforceInvariant() {
        let serialized = markdown()
        let parsed = MentionLink.parse(serialized)

        var doomed: [Range<AttributedString.Index>] = []
        var offset = 0

        // Keyed run view, for the same reason as `markdown()`: a chip split by
        // an unrelated attribute must be judged as ONE mention, or clause 1 fails
        // on every fragment and a visually intact chip dies for no reason.
        for (mention, range) in attributed.runs[MentionAttribute.self] {
            let visible = String(attributed[range].characters)
            guard let mention else {
                offset += visible.count
                continue
            }

            let link = MentionLink.serialize(path: mention.path, isDir: mention.isDir)
            let expected = offset..<(offset + link.count)
            let clauseOne = visible == "@" + MentionLink.basename(of: mention.path)
            let clauseTwo = parsed.contains { candidate in
                candidate.range == expected
                    && candidate.path == mention.path
                    && candidate.isDir == mention.isDir
            }
            if !clauseOne || !clauseTwo {
                doomed.append(range)
            }
            offset += link.count
        }

        for range in doomed {
            attributed[range][MentionAttribute.self] = nil
        }
    }

    // MARK: Mutation

    mutating func apply(command: String, over range: Range<Int>,
                        selection: inout AttributedTextSelection) {
        let suppressed = hasSeparator(after: range.upperBound)
        let separator = suppressed ? "" : " "
        replace(range, with: AttributedString("/\(command)\(separator)"),
               selection: &selection, skipExistingSeparator: suppressed)
    }

    mutating func apply(path: String, isDir: Bool, over range: Range<Int>,
                        selection: inout AttributedTextSelection) {
        var chip = AttributedString("@" + MentionLink.basename(of: path))
        if MentionLink.isSafe(path) {
            chip[MentionAttribute.self] = MentionValue(path: path, isDir: isDir)
        }
        var replacement = chip
        let suppressed = hasSeparator(after: range.upperBound)
        if !suppressed {
            replacement.append(AttributedString(" "))
        }
        replace(range, with: replacement, selection: &selection, skipExistingSeparator: suppressed)
    }

    /// True when the character right after the replaced range — in the text
    /// as it stands BEFORE this pick — is whitespace other than a newline.
    /// Matches the desktop rule exactly, including the newline exclusion
    /// (crates/ui/src/composer.rs:1483-1489): a pick never doubles up a space
    /// that is already there, but a hard newline right after the token still
    /// gets its own trailing space rather than folding onto the next line.
    private func hasSeparator(after upper: Int) -> Bool {
        let count = attributed.characters.count
        let clamped = max(0, min(count, upper))
        guard clamped < count else { return false }
        let index = attributed.index(attributed.startIndex, offsetByCharacters: clamped)
        let ch = attributed.characters[index]
        return ch.isWhitespace && ch != "\n" && ch != "\r"
    }

    mutating func clear(selection: inout AttributedTextSelection) {
        attributed = AttributedString("")
        selection = AttributedTextSelection(insertionPoint: attributed.startIndex)
    }

    /// Every text mutation goes through here so the invariant is re-checked
    /// exactly once per change. `transform(updating:)` keeps `selection` valid
    /// across the edit; the final caret is then placed deliberately after the
    /// inserted text, so that assignment — not the `updating:` argument — is what
    /// determines where the caret ends up.
    ///
    /// `skipExistingSeparator` is the other half of the desktop's separator
    /// rule (crates/ui/src/composer.rs:1489): when a pick suppresses its own
    /// trailing space because one is already there, the caret still lands
    /// PAST that pre-existing character, not just before it. Landing before
    /// it would glue the next keystroke onto the token, and — for the picks
    /// the mention veto does not cover (commands; unsafe paths, which carry
    /// no attribute) — would leave the caret sitting exactly on the token's
    /// own trigger boundary, reopening the popover on the pick that just
    /// resolved it.
    private mutating func replace(_ range: Range<Int>, with replacement: AttributedString,
                                  selection: inout AttributedTextSelection,
                                  skipExistingSeparator: Bool = false) {
        let count = attributed.characters.count
        let lower = max(0, min(count, range.lowerBound))
        let upper = max(lower, min(count, range.upperBound))

        attributed.transform(updating: &selection) { text in
            let start = text.index(text.startIndex, offsetByCharacters: lower)
            let end = text.index(text.startIndex, offsetByCharacters: upper)
            text.replaceSubrange(start..<end, with: replacement)
        }
        enforceInvariant()

        let advance = replacement.characters.count + (skipExistingSeparator ? 1 : 0)
        let caret = attributed.index(attributed.startIndex,
                                     offsetByCharacters: lower + advance)
        selection = AttributedTextSelection(insertionPoint: caret)
    }

    // MARK: Helpers

    /// (lowerOffset, upperOffset) for each mention span. The keyed view keeps a
    /// chip that an unrelated attribute has split reported as one span, so the
    /// trigger veto covers the whole chip rather than one fragment of it.
    private func mentionRuns() -> [(Int, Int)] {
        var out: [(Int, Int)] = []
        for (mention, range) in attributed.runs[MentionAttribute.self] where mention != nil {
            let lower = attributed.characters.distance(from: attributed.startIndex,
                                                       to: range.lowerBound)
            let upper = attributed.characters.distance(from: attributed.startIndex,
                                                       to: range.upperBound)
            out.append((lower, upper))
        }
        return out
    }
}

#if DEBUG
extension ComposerText {
    /// Simulate a raw user edit: replace a character range and re-check the
    /// invariant, exactly as the text view's binding write does. Tests only.
    mutating func replaceForTesting(_ range: Range<Int>, with plain: String) {
        var selection = AttributedTextSelection(insertionPoint: attributed.startIndex)
        replace(range, with: AttributedString(plain), selection: &selection)
    }
}
#endif
