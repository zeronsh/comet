// Trigger rules ported from the desktop composer: slash_token
// (crates/ui/src/composer.rs:3212-3233) and mention_token (3179-3208).
// Desktop and iOS must agree on what counts as a trigger, or the same draft
// behaves differently on two clients.

import XCTest
@testable import Zeron

final class ComposerTriggerTests: XCTestCase {

    // MARK: Command

    func testSlashAtStartTriggers() {
        let t = ComposerTrigger.detect(text: "/td", caret: 3)
        XCTAssertEqual(t, Trigger(kind: .command, query: "td", token: "/td", range: 0..<3))
    }

    func testSlashNotAtStartDoesNotTrigger() {
        // Slash commands are whole-prompt prefixes: only offset 0 counts.
        XCTAssertNil(ComposerTrigger.detect(text: "hi /td", caret: 6))
        XCTAssertNil(ComposerTrigger.detect(text: "hi\n/td", caret: 6))
    }

    func testCaretPastCommandTokenDoesNotTrigger() {
        // Typing the argument closes the popup.
        XCTAssertNil(ComposerTrigger.detect(text: "/goal ship it", caret: 13))
    }

    func testCaretInsideCommandTokenTriggersWithFullTokenRange() {
        let t = ComposerTrigger.detect(text: "/goal ship it", caret: 3)
        XCTAssertEqual(t, Trigger(kind: .command, query: "go", token: "/goal", range: 0..<5))
    }

    func testCommandQueryWithSlashDoesNotTrigger() {
        // A typed path must not open the command popup.
        XCTAssertNil(ComposerTrigger.detect(text: "/src/main", caret: 9))
    }

    func testCaretAtZeroDoesNotTrigger() {
        XCTAssertNil(ComposerTrigger.detect(text: "/td", caret: 0))
    }

    // MARK: Path

    func testAtBeginningTokenTriggers() {
        let t = ComposerTrigger.detect(text: "look @Inf", caret: 9)
        XCTAssertEqual(t, Trigger(kind: .path, query: "Inf", token: "@Inf", range: 5..<9))
    }

    func testEmailDoesNotTrigger() {
        XCTAssertNil(ComposerTrigger.detect(text: "name@example.com", caret: 16))
    }

    func testOpenParenBoundaryTriggers() {
        let t = ComposerTrigger.detect(text: "(@src", caret: 5)
        XCTAssertEqual(t, Trigger(kind: .path, query: "src", token: "@src", range: 1..<5))
    }

    func testSecondAtInQueryDoesNotTrigger() {
        XCTAssertNil(ComposerTrigger.detect(text: "@a@b", caret: 4))
    }

    func testPathRangeExtendsPastCaretToWhitespace() {
        // Editing the middle of a mention replaces the whole token.
        let t = ComposerTrigger.detect(text: "@Info.plist tail", caret: 5)
        XCTAssertEqual(t, Trigger(kind: .path, query: "Info", token: "@Info.plist", range: 0..<11))
    }

    func testAtWithNoTriggerReturnsNil() {
        XCTAssertNil(ComposerTrigger.detect(text: "plain words", caret: 11))
    }

    // MARK: Replace

    func testReplaceSubstitutesRangeAndMovesCaret() {
        let result = ComposerTrigger.replace("look @Inf here", range: 5..<9, with: "@Info.plist ")
        XCTAssertEqual(result.text, "look @Info.plist  here")
        XCTAssertEqual(result.caret, 17)
    }

    func testReplaceClampsOutOfRangeBounds() {
        let result = ComposerTrigger.replace("ab", range: 1..<99, with: "X")
        XCTAssertEqual(result.text, "aX")
        XCTAssertEqual(result.caret, 2)
    }

    func testOffsetsCountCharactersNotBytes() {
        // "é" and "🙂" are one Character each.
        let t = ComposerTrigger.detect(text: "é🙂 @a", caret: 5)
        XCTAssertEqual(t, Trigger(kind: .path, query: "a", token: "@a", range: 3..<5))
    }
}
