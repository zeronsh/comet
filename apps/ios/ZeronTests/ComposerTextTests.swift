// The rich-text composer model. These tests pin the two-clause invariant:
// a run keeps its mention attribute only if (1) its visible text is exactly
// "@basename" and (2) the serialized draft re-parses to that mention at that
// position. Neither clause implies the other — see the spec's "round-trip
// invariant" section.

import XCTest
// AttributedTextSelection is a SwiftUI type, and `@testable import` does not
// re-export the module's own imports. Without this the test file will not
// compile.
import SwiftUI
@testable import Zeron

final class ComposerTextTests: XCTestCase {

    /// A draft reading "see @a.rs " with the chip attributed.
    private func draftWithChip(prefix: String = "see ",
                               path: String = "a.rs",
                               isDir: Bool = false) -> ComposerText {
        var text = ComposerText(prefix)
        var selection = AttributedTextSelection(insertionPoint: text.attributed.endIndex)
        let end = text.plainText.count
        text.apply(path: path, isDir: isDir, over: end..<end, selection: &selection)
        return text
    }

    // MARK: Serialization

    func testIntactChipSerializesToTheCanonicalLink() {
        let text = draftWithChip()
        XCTAssertEqual(text.markdown(), "see [a.rs](zeron-file:a.rs) ")
    }

    func testPlainTextShowsTheChipAsAtBasename() {
        XCTAssertEqual(draftWithChip().plainText, "see @a.rs ")
    }

    func testDirectoryPickSerializesWithTrailingSlash() {
        let text = draftWithChip(prefix: "", path: "src/one", isDir: true)
        XCTAssertEqual(text.markdown(), "[one](zeron-file:src/one/) ")
    }

    func testDraftWithNoChipsSerializesUnchanged() {
        XCTAssertEqual(ComposerText("plain words").markdown(), "plain words")
    }

    // MARK: Clause 1 — visible text

    func testTypingInsideAChipDropsTheAttribute() {
        var text = draftWithChip()          // "see @a.rs "
        text.replaceForTesting(6..<6, with: "X")   // "see @aX.rs "
        XCTAssertEqual(text.markdown(), "see @aX.rs ")
    }

    func testBackspaceAtTheChipEdgeDropsTheAttribute() {
        var text = draftWithChip()          // "see @a.rs "
        text.replaceForTesting(8..<9, with: "")   // "see @a.r "
        XCTAssertEqual(text.markdown(), "see @a.r ")
    }

    func testDeletingTheSpaceBetweenTwoSameFileChipsDropsBoth() {
        var text = draftWithChip(prefix: "")            // "@a.rs "
        var selection = AttributedTextSelection(insertionPoint: text.attributed.endIndex)
        let end = text.plainText.count
        text.apply(path: "a.rs", isDir: false, over: end..<end, selection: &selection)
        XCTAssertEqual(text.plainText, "@a.rs @a.rs ")
        text.replaceForTesting(5..<6, with: "")         // "@a.rs@a.rs "
        XCTAssertEqual(text.markdown(), "@a.rs@a.rs ")
    }

    func testAnUntouchedChipSurvivesAnEditElsewhere() {
        var text = draftWithChip()
        text.replaceForTesting(0..<0, with: "hey ")
        XCTAssertEqual(text.markdown(), "hey see [a.rs](zeron-file:a.rs) ")
    }

    func testTextTypedAfterAChipDoesNotInheritTheAttribute() {
        var text = draftWithChip()               // "see @a.rs "
        text.replaceForTesting(9..<9, with: "Z") // "see @a.rsZ "
        // `inheritedByAddedText = false` means "Z" lands OUTSIDE the run, so
        // the chip still reads exactly "@a.rs" and both clauses hold. The chip
        // survives and "Z" is plain text beside it. If this ever asserts the
        // chip was dropped, the attribute is bleeding into typed text.
        XCTAssertEqual(text.markdown(), "see [a.rs](zeron-file:a.rs)Z ")
        XCTAssertEqual(text.plainText, "see @a.rsZ ")
    }

    // MARK: Clause 2 — round trip

    /// THE REVIEW'S FAILURE CASE.
    func testStrayOpenBracketBeforeAChipDropsThatChip() {
        var text = draftWithChip(prefix: "see ")   // "see @a.rs "
        text.replaceForTesting(4..<4, with: "[ notes ")  // "see [ notes @a.rs "
        XCTAssertEqual(text.markdown(), "see [ notes @a.rs ")
    }

    func testAnUnsafePathNeverGetsAnAttribute() {
        var text = ComposerText("")
        var selection = AttributedTextSelection(insertionPoint: text.attributed.startIndex)
        text.apply(path: "../secret", isDir: false, over: 0..<0, selection: &selection)
        XCTAssertEqual(text.markdown(), "@secret ")
    }

    // MARK: Run splitting

    func testAnUnrelatedAttributeSplittingAChipDoesNotDuplicateItsLink() {
        var text = draftWithChip()          // "see @a.rs "
        // Colour two characters in the middle of the chip. Foundation's plain
        // `runs` view splits at that boundary; the mention-keyed view must not.
        // Iterating the plain view emits the link once PER FRAGMENT, and clause 1
        // then fails on every fragment and kills a visually intact chip.
        let start = text.attributed.index(text.attributed.startIndex, offsetByCharacters: 6)
        let end = text.attributed.index(text.attributed.startIndex, offsetByCharacters: 8)
        text.attributed[start..<end].foregroundColor = .red

        XCTAssertEqual(text.markdown(), "see [a.rs](zeron-file:a.rs) ",
                       "a split chip must serialize to ONE link, not one per fragment")
        text.enforceInvariant()
        XCTAssertEqual(text.markdown(), "see [a.rs](zeron-file:a.rs) ",
                       "a visually intact chip must survive an unrelated attribute")
    }

    // MARK: Trigger veto

    func testCaretInsideAnIntactChipOpensNoTrigger() {
        let text = draftWithChip()          // "see @a.rs "
        let index = text.attributed.index(text.attributed.startIndex, offsetByCharacters: 7)
        XCTAssertNil(text.trigger(at: AttributedTextSelection(insertionPoint: index)))
    }

    func testCaretAtTheChipTrailingEdgeOpensNoTrigger() {
        let text = draftWithChip()
        let index = text.attributed.index(text.attributed.startIndex, offsetByCharacters: 9)
        XCTAssertNil(text.trigger(at: AttributedTextSelection(insertionPoint: index)))
    }

    func testAFreshlyTypedAtStillTriggers() {
        let text = ComposerText("look @Inf")
        let index = text.attributed.index(text.attributed.startIndex, offsetByCharacters: 9)
        XCTAssertEqual(text.trigger(at: AttributedTextSelection(insertionPoint: index))?.query, "Inf")
    }

    // MARK: Index adapter

    func testCaretOffsetCountsCharactersAcrossMultibyteText() {
        let text = ComposerText("é🙂ab")
        let index = text.attributed.index(text.attributed.startIndex, offsetByCharacters: 3)
        XCTAssertEqual(text.caretOffset(AttributedTextSelection(insertionPoint: index)), 3)
    }

    func testARangeSelectionHasNoCaretAndNoTrigger() {
        let text = ComposerText("/td")
        let start = text.attributed.startIndex
        let end = text.attributed.index(start, offsetByCharacters: 3)
        let selection = AttributedTextSelection(range: start..<end)
        XCTAssertNil(text.caretOffset(selection))
        XCTAssertNil(text.trigger(at: selection))
    }

    // MARK: Command insertion

    func testCommandPickInsertsPlainTextWithNoAttribute() {
        var text = ComposerText("/td")
        var selection = AttributedTextSelection(insertionPoint: text.attributed.endIndex)
        text.apply(command: "tdd", over: 0..<3, selection: &selection)
        XCTAssertEqual(text.plainText, "/tdd ")
        XCTAssertEqual(text.markdown(), "/tdd ")
    }

    // MARK: Separator suppression (desktop composer.rs:1483-1489)

    /// THE REVIEW'S FAILURE CASE: picking mid-draft used to double the space.
    func testPathPickDoesNotDoubleAnExistingSeparator() {
        var text = ComposerText("check @ma out")
        var selection = AttributedTextSelection(insertionPoint: text.attributed.startIndex)
        // "@ma" spans offsets 6..<9; a space already sits at offset 9.
        text.apply(path: "main.rs", isDir: false, over: 6..<9, selection: &selection)
        XCTAssertEqual(text.plainText, "check @main.rs out")
    }

    /// A newline right after the token is excluded from the desktop rule, so
    /// the pick still gets its own trailing space rather than folding onto it.
    func testPathPickAddsASeparatorBeforeANewline() {
        var text = ComposerText("check @ma\nout")
        var selection = AttributedTextSelection(insertionPoint: text.attributed.startIndex)
        text.apply(path: "main.rs", isDir: false, over: 6..<9, selection: &selection)
        XCTAssertEqual(text.plainText, "check @main.rs \nout")
    }

    func testCommandPickDoesNotDoubleAnExistingSeparator() {
        var text = ComposerText("/td more")
        var selection = AttributedTextSelection(insertionPoint: text.attributed.startIndex)
        text.apply(command: "tdd", over: 0..<3, selection: &selection)
        XCTAssertEqual(text.plainText, "/tdd more")
    }

    /// A command pick carries no mention attribute, so nothing else stops the
    /// popover from reopening the instant the caret lands back on the token's
    /// own trigger boundary. The caret has to clear that boundary — desktop
    /// does this by landing PAST the separator it declined to duplicate
    /// (composer.rs:1489's `existing_separator.map(char::len_utf8)`), not just
    /// before it.
    func testCommandPickAdvancesTheCaretPastAnExistingSeparatorSoItDoesNotReopen() {
        var text = ComposerText("/td more")
        var selection = AttributedTextSelection(insertionPoint: text.attributed.startIndex)
        text.apply(command: "tdd", over: 0..<3, selection: &selection)
        XCTAssertNil(text.trigger(at: selection))
    }

    /// Same rule, for the other case the mention veto does not cover: an
    /// unsafe path carries no attribute either, so it needs the same caret
    /// hop as a command pick.
    func testUnsafePathPickAdvancesTheCaretPastAnExistingSeparatorSoItDoesNotReopen() {
        var text = ComposerText("open @x more")
        var selection = AttributedTextSelection(insertionPoint: text.attributed.startIndex)
        text.apply(path: "../secret", isDir: false, over: 5..<7, selection: &selection)
        XCTAssertEqual(text.plainText, "open @secret more")
        XCTAssertNil(text.trigger(at: selection))
    }
}
