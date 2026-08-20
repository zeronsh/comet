// The suggestion store. Both fetchers are injected closures so these run with
// no socket. The rules pinned here mirror the desktop composer: a cache key
// that includes the device (composer.rs:3236-3241), a dismissal that survives
// caret moves inside one token (composer.rs:3301-3304), and error text that is
// never rendered as "no results" (composer.rs:3296-3300).

import XCTest
@testable import Zeron

@MainActor
final class ComposerSuggestionsTests: XCTestCase {

    private let context = SuggestionContext(harness: "claude-code", deviceId: "dev-a",
                                            cwd: "~/proj", chatId: "chat-1", spaceId: nil)

    private func command(_ name: String) -> SlashCommand {
        SlashCommand(name: name, description: "does \(name)", inputHint: nil)
    }

    func testCommandsAreFetchedOnceAndServedFromCache() async {
        var calls = 0
        let store = ComposerSuggestions()
        store.fetchCommands = { _, _, _ in
            calls += 1
            return [self.command("tdd"), self.command("plan")]
        }

        await store.update(trigger: Trigger(kind: .command, query: "", token: "/", range: 0..<1),
                           context: context)
        XCTAssertEqual(store.items.map(\.label), ["/tdd", "/plan"])

        await store.update(trigger: Trigger(kind: .command, query: "t", token: "/t", range: 0..<2),
                           context: context)
        XCTAssertEqual(store.items.map(\.label), ["/tdd"])
        XCTAssertEqual(calls, 1, "a cached list must not re-fetch on every keystroke")
    }

    func testCommandCacheKeyIncludesTheDevice() async {
        var seen: [String] = []
        let store = ComposerSuggestions()
        store.fetchCommands = { _, device, _ in
            seen.append(device)
            return [self.command("tdd")]
        }
        let trigger = Trigger(kind: .command, query: "", token: "/", range: 0..<1)

        await store.update(trigger: trigger, context: context)
        var other = context
        other.deviceId = "dev-b"
        await store.update(trigger: trigger, context: other)

        XCTAssertEqual(seen, ["dev-a", "dev-b"],
                       "two devices share the path ~ for every project-less chat")
    }

    func testCommandFilterMatchesNameBeforeDescription() async {
        let store = ComposerSuggestions()
        store.fetchCommands = { _, _, _ in
            [SlashCommand(name: "plan", description: "no match", inputHint: nil),
             SlashCommand(name: "zzz", description: "make a plan", inputHint: nil)]
        }
        await store.update(trigger: Trigger(kind: .command, query: "plan", token: "/plan", range: 0..<5),
                           context: context)
        XCTAssertEqual(store.items.map(\.label), ["/plan", "/zzz"])
    }

    func testPathResultsKeepHostOrder() async {
        let store = ComposerSuggestions()
        store.fetchPaths = { _, _ in
            [FileSearchMatch(path: "z.rs", isDir: false),
             FileSearchMatch(path: "a.rs", isDir: false)]
        }
        await store.update(trigger: Trigger(kind: .path, query: "rs", token: "@rs", range: 0..<3),
                           context: context)
        XCTAssertEqual(store.items.map(\.label), ["z.rs", "a.rs"],
                       "the host already ranked these; do not re-sort")
    }

    func testAFailureShowsAnErrorNotAnEmptyList() async {
        let store = ComposerSuggestions()
        store.fetchPaths = { _, _ in throw RelayError.hostOffline }
        await store.update(trigger: Trigger(kind: .path, query: "a", token: "@a", range: 0..<2),
                           context: context)
        XCTAssertTrue(store.items.isEmpty)
        XCTAssertEqual(store.errorText, "The session's device is unreachable")
    }

    func testVersionSkewGetsItsOwnMessage() async {
        let store = ComposerSuggestions()
        store.fetchPaths = { _, _ in throw RelayError.rpc("unknown method: SearchFiles") }
        await store.update(trigger: Trigger(kind: .path, query: "a", token: "@a", range: 0..<2),
                           context: context)
        XCTAssertEqual(store.errorText,
                       "The session's device runs an older zeron — update it to search its files")
    }

    func testDismissSurvivesACaretMoveInsideTheSameToken() async {
        let store = ComposerSuggestions()
        store.fetchCommands = { _, _, _ in [self.command("tdd")] }
        let trigger = Trigger(kind: .command, query: "td", token: "/td", range: 0..<3)

        await store.update(trigger: trigger, context: context)
        store.dismiss(trigger)

        // Same token, caret moved back one: still dismissed. `query` changed
        // and `token` did not, which is exactly why the key is `token`.
        await store.update(trigger: Trigger(kind: .command, query: "t", token: "/td", range: 0..<3),
                           context: context)
        XCTAssertTrue(store.items.isEmpty)

        // A same-LENGTH edit: the span is unchanged but the token is not, so
        // this reopens. A range-only key would wrongly stay closed here. The
        // query still matches the fixture, so an empty list would mean the
        // dismissal held — not that the filter came up dry.
        await store.update(trigger: Trigger(kind: .command, query: "Td", token: "/Td", range: 0..<3),
                           context: context)
        XCTAssertEqual(store.items.map(\.label), ["/tdd"])
        store.dismiss(Trigger(kind: .command, query: "Td", token: "/Td", range: 0..<3))

        // The token grew: an edit reopens it.
        await store.update(trigger: Trigger(kind: .command, query: "tdd", token: "/tdd", range: 0..<4),
                           context: context)
        XCTAssertEqual(store.items.map(\.label), ["/tdd"])
    }

    // Regression: a cache hit that ends its request synchronously must clear
    // isLoading itself. If it does not, this interleaving strands the spinner
    // on forever: an in-flight miss (request B) sets isLoading = true and
    // suspends; before it resolves, a cache hit for an already-primed key
    // (request A) supersedes it and returns without ever awaiting; B then
    // resolves, loses its generation check against A, and correctly bails out
    // WITHOUT touching isLoading — so nothing is left to clear it unless A's
    // cache-hit branch owned that job itself.
    //
    // The interleaving is driven directly, not with a sleep or a yield: A is
    // re-entrantly called from inside B's own fetcher closure, while B is
    // genuinely suspended on that same await.
    func testSupersededCacheHitClearsTheSpinner() async {
        let store = ComposerSuggestions()
        var reentrantSawLoading = false

        // Prime the cache for context A so a later hit on it resolves
        // synchronously, without ever awaiting.
        store.fetchCommands = { _, _, _ in [self.command("tdd")] }
        let triggerA = Trigger(kind: .command, query: "", token: "/", range: 0..<1)
        await store.update(trigger: triggerA, context: context)

        var contextB = context
        contextB.cwd = "~/other"
        let triggerB = Trigger(kind: .command, query: "", token: "/", range: 0..<1)

        // The B fetch is a cache miss: it sets isLoading = true, then suspends
        // here, in flight. From inside that suspension, re-enter update() for
        // the already-cached A key.
        store.fetchCommands = { _, _, _ in
            reentrantSawLoading = store.isLoading
            await store.update(trigger: triggerA, context: self.context)
            return [self.command("tdd")]
        }

        await store.update(trigger: triggerB, context: contextB)

        XCTAssertTrue(reentrantSawLoading,
                      "the B request must genuinely be in flight when A re-enters")
        XCTAssertFalse(store.isLoading,
                       "a superseded cache hit must not strand the spinner on")
    }
}
