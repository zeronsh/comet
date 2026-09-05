// Conformance vectors for the mention link codec. Every expected value here
// is lifted from the Rust tests in crates/ui/src/composer.rs; the two
// implementations must agree byte for byte or a chip written on the phone
// renders as raw Markdown on the desktop.

import XCTest
@testable import Zeron

final class MentionLinkTests: XCTestCase {

    // MARK: Encoding

    func testPercentEncodeLeavesUnreservedBytes() {
        XCTAssertEqual(MentionLink.percentEncode("src/a-b_c.d~e"), "src/a-b_c.d~e")
    }

    func testPercentEncodeUsesUppercaseHex() {
        XCTAssertEqual(MentionLink.percentEncode("a b"), "a%20b")
        XCTAssertEqual(MentionLink.percentEncode("a\nb"), "a%0Ab")
    }

    func testPercentEncodeIsPerByteForMultibyte() {
        XCTAssertEqual(MentionLink.percentEncode("é"), "%C3%A9")
    }

    func testPercentDecodeRoundTrips() {
        for raw in ["src/a.rs", "a b", "é", "a\nb", "sr(c)/x"] {
            XCTAssertEqual(MentionLink.percentDecode(MentionLink.percentEncode(raw)), raw)
        }
    }

    func testPercentDecodeRejectsTruncatedEscape() {
        XCTAssertNil(MentionLink.percentDecode("a%2"))
        XCTAssertNil(MentionLink.percentDecode("a%zz"))
    }

    // MARK: Escaping

    func testEscapeLabelEscapesBackslashFirst() {
        XCTAssertEqual(MentionLink.escapeLabel("a\\b"), "a\\\\b")
        XCTAssertEqual(MentionLink.escapeLabel("a[b]c"), "a\\[b\\]c")
    }

    // MARK: Safety

    func testIsSafeRejectsUnsafePaths() {
        XCTAssertFalse(MentionLink.isSafe(""))
        XCTAssertFalse(MentionLink.isSafe("/abs/path"))
        XCTAssertFalse(MentionLink.isSafe("src\\win"))
        XCTAssertFalse(MentionLink.isSafe("../a.rs"))
        XCTAssertFalse(MentionLink.isSafe("src/./a.rs"))
        XCTAssertFalse(MentionLink.isSafe("src//a.rs"))
        XCTAssertFalse(MentionLink.isSafe("src/a\nb.rs"))
    }

    func testIsSafeAcceptsOrdinaryRelativePaths() {
        XCTAssertTrue(MentionLink.isSafe("a.rs"))
        XCTAssertTrue(MentionLink.isSafe("src/one/mod.rs"))
        XCTAssertTrue(MentionLink.isSafe("src/a file.rs"))
    }

    // MARK: Serialize

    func testSerializeFile() {
        XCTAssertEqual(MentionLink.serialize(path: "src/a.rs", isDir: false),
                       "[a.rs](zeron-file:src/a.rs)")
    }

    func testSerializeDirectoryCarriesTrailingSlash() {
        XCTAssertEqual(MentionLink.serialize(path: "src/one", isDir: true),
                       "[one](zeron-file:src/one/)")
    }

    func testSerializeEscapesLabelAndEncodesPath() {
        XCTAssertEqual(MentionLink.serialize(path: "src/a b.rs", isDir: false),
                       "[a b.rs](zeron-file:src/a%20b.rs)")
    }

    // MARK: Parse — these mirror the Rust rejection assertions at composer.rs:5955-5961

    func testParseAcceptsAWellFormedLink() {
        let parsed = MentionLink.parse("see [a.rs](zeron-file:src/a.rs) ok")
        XCTAssertEqual(parsed, [ParsedMention(range: 4..<31, basename: "a.rs",
                                              path: "src/a.rs", isDir: false)])
    }

    func testParseRejectsForeignScheme() {
        XCTAssertTrue(MentionLink.parse("[site](https://example.com/a)").isEmpty)
    }

    func testParseRejectsUnsafePath() {
        XCTAssertTrue(MentionLink.parse("[a.rs](zeron-file:../a.rs)").isEmpty)
        XCTAssertTrue(MentionLink.parse("[a.rs](zeron-file:src%5Cfake%5Ca.rs)").isEmpty)
        XCTAssertTrue(MentionLink.parse("[a.rs](zeron-file:src/a%0A.rs)").isEmpty)
    }

    func testParseRejectsUnencodedPath() {
        // percent_encode_path(&target) == encoded — a raw space fails.
        XCTAssertTrue(MentionLink.parse("[a file.rs](zeron-file:src/a file.rs)").isEmpty)
    }

    func testParseRejectsLabelThatIsNotTheBasename() {
        XCTAssertTrue(MentionLink.parse("[other](zeron-file:src/a.rs)").isEmpty)
    }

    func testParseAcceptsADirectory() {
        let parsed = MentionLink.parse("[one](zeron-file:src/one/)")
        XCTAssertEqual(parsed, [ParsedMention(range: 0..<26, basename: "one",
                                              path: "src/one", isDir: true)])
    }

    /// THE REVIEW'S FAILURE CASE. A stray `[` before a chip makes the parser
    /// consume the chip's `](` as the close of the earlier label, and line 792
    /// of the Rust then skips past the whole link. Nothing is found.
    func testStrayOpenBracketSwallowsAFollowingChip() {
        XCTAssertTrue(MentionLink.parse("see [ notes [a.rs](zeron-file:a.rs)").isEmpty)
    }

    func testParseFindsTwoChips() {
        let text = "[a.rs](zeron-file:a.rs) and [b.rs](zeron-file:b.rs)"
        XCTAssertEqual(MentionLink.parse(text).map(\.path), ["a.rs", "b.rs"])
    }
}
