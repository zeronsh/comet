// Suggestion state for the composer popover: what to show, whether it is
// loading, and why it failed. Works on plain values only — no AttributedString
// — so it stays testable with no socket and no simulator UI.
//
// Freshness is stale-while-revalidate: a cached list draws at once and one
// request per popover open refreshes it. This type holds NO TTL. The engine's
// command cache owns expiry (crates/engine/src/commands.rs), so there is one
// expiry policy, in one place.

import Foundation

struct SuggestionItem: Identifiable, Equatable {
    enum Payload: Equatable {
        case command(String)
        case path(String, isDir: Bool)
    }

    let id: String
    let label: String
    let detail: String
    let payload: Payload
}

/// Everything the fetchers need that the trigger does not carry.
struct SuggestionContext: Equatable {
    var harness: String
    var deviceId: String
    /// A path on the HOST device. `~` travels unexpanded.
    var cwd: String
    var chatId: String?
    var spaceId: String?
}

/// Where the popover's commands come from: the chat's own directory, else the
/// picked space's folder, else the host's home.
///
/// Two rules come from the desktop's `slash_cwd` (crates/ui/src/composer.rs):
/// a blank path counts as absent, and a draft with no chat falls to the space's
/// folder. The checkout plan is ignored for the same reason the desktop ignores
/// it — a draft that will mint a fresh worktree has no directory yet when the
/// popover opens, and a worktree of the same repo carries the same tracked
/// commands anyway.
///
/// One rule does NOT come from the desktop: a chat whose own `cwd` is absent
/// falls through to its space's folder here, where the desktop stops at `~`.
/// That is what this composer already did before the draft case existed, and
/// the space's folder is the better answer of the two.
///
/// Blank counts as absent because `listCommands` drops an empty `cwd` from the
/// params, and the host then answers for its home — a different list than the
/// space's.
func slashCwd(chatCwd: String?, spacePath: String?) -> String {
    func present(_ value: String?) -> String? {
        guard let value, !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return nil }
        return value
    }
    return present(chatCwd) ?? present(spacePath) ?? "~"
}

/// Cache identity for one command list. The device belongs in the key because
/// every project-less chat, on every device, shares the path `~`
/// (crates/ui/src/composer.rs:3236-3241).
struct CommandKey: Hashable {
    let harness: String
    let device: String
    let cwd: String
}

@Observable
@MainActor
final class ComposerSuggestions {

    private(set) var items: [SuggestionItem] = []
    private(set) var isLoading = false
    private(set) var errorText: String?

    /// Injected so tests run with no socket. Wired to WorkspaceStore at the
    /// call site.
    var fetchCommands: (_ harness: String, _ device: String, _ cwd: String) async throws -> [SlashCommand] = { _, _, _ in [] }
    var fetchPaths: (_ context: SuggestionContext, _ query: String) async throws -> [FileSearchMatch] = { _, _ in [] }

    private var commandCache: [CommandKey: [SlashCommand]] = [:]
    /// The kind the current `items` belong to. Switching kinds must clear them:
    /// otherwise the popover reopens instantly with the PREVIOUS kind's rows
    /// under the new kind's header, and keeps them — tappable — for the whole
    /// RPC round trip. `isLoading` does not mask it, because ComposerPopover
    /// only shows its loading line when `items` is empty.
    private var lastKind: TriggerKind?
    /// The token the user closed the popover on: its span AND its full text.
    /// Keyed on both because a caret move inside a dismissed token must keep it
    /// closed, while any edit reopens it (composer.rs:3301-3304). The span alone
    /// would misread a same-length replacement as "unchanged"; the text alone
    /// would misread the same token typed twice.
    private var dismissed: (range: Range<Int>, token: String)?
    private var generation = 0

    func dismiss(_ trigger: Trigger) {
        dismissed = (trigger.range, trigger.token)
        items = []
        errorText = nil
        isLoading = false
    }

    /// Drop everything. Called when the trigger closes, so a later trigger of
    /// the same kind cannot reopen onto the previous query's rows.
    func reset() {
        items = []
        errorText = nil
        isLoading = false
        lastKind = nil
        generation += 1
    }

    /// Mark a fetch as pending the moment a trigger appears, before the
    /// debounce elapses. Without this the popover renders its no-match line in
    /// the window between the keystroke and the first request.
    func beginPending() {
        isLoading = true
        errorText = nil
    }

    func update(trigger: Trigger, context: SuggestionContext) async {
        // Bumped BEFORE the dismissed check, not after: the dismissed branch
        // below returns early, and an in-flight request from before the
        // dismissal is still reading the OLD generation. Without this bump
        // that request stays "current", writes `items` once it lands, and
        // reopens a popover the user just closed.
        generation += 1

        if let dismissed, dismissed.range == trigger.range, dismissed.token == trigger.token {
            items = []
            // Clearing here matters: a superseded in-flight request returns at
            // its own generation guard WITHOUT touching isLoading (correctly —
            // it must not clobber a newer request's state), so any path that
            // ends a request synchronously has to clear the spinner itself.
            isLoading = false
            return
        }
        dismissed = nil

        if lastKind != trigger.kind {
            items = []
        }
        lastKind = trigger.kind

        let mine = generation
        errorText = nil

        switch trigger.kind {
        case .command:
            await loadCommands(trigger: trigger, context: context, generation: mine)
        case .path:
            await loadPaths(trigger: trigger, context: context, generation: mine)
        }
    }

    // MARK: Commands

    private func loadCommands(trigger: Trigger, context: SuggestionContext,
                              generation mine: Int) async {
        let key = CommandKey(harness: context.harness, device: context.deviceId, cwd: context.cwd)

        if let cached = commandCache[key] {
            items = Self.filter(cached, query: trigger.query)
            // Same reason as the dismissed path: this ends the request without
            // ever awaiting, so it owns clearing the spinner. Omitting it strands
            // isLoading == true forever when this hit supersedes an in-flight miss.
            isLoading = false
            return
        }

        isLoading = true
        do {
            let fetched = try await fetchCommands(context.harness, context.deviceId, context.cwd)
            guard mine == generation else { return }
            commandCache[key] = fetched
            items = Self.filter(fetched, query: trigger.query)
        } catch {
            guard mine == generation else { return }
            items = []
            errorText = Self.commandError(error)
        }
        isLoading = false
    }

    /// Name matches first, then description matches. The host returns the full
    /// list, so filtering is the client's job.
    private static func filter(_ commands: [SlashCommand], query: String) -> [SuggestionItem] {
        let needle = query.lowercased()
        var byName: [SuggestionItem] = []
        var byDescription: [SuggestionItem] = []
        for command in commands {
            let item = SuggestionItem(id: "cmd:\(command.name)",
                                      label: "/\(command.name)",
                                      detail: command.description,
                                      payload: .command(command.name))
            if needle.isEmpty || command.name.lowercased().contains(needle) {
                byName.append(item)
            } else if command.description.lowercased().contains(needle) {
                byDescription.append(item)
            }
        }
        return byName + byDescription
    }

    // MARK: Paths

    private func loadPaths(trigger: Trigger, context: SuggestionContext,
                           generation mine: Int) async {
        isLoading = true
        do {
            let matches = try await fetchPaths(context, trigger.query)
            guard mine == generation else { return }
            // The host already ranked and capped these (repos.rs:1082).
            items = matches.map { match in
                SuggestionItem(id: "path:\(match.path)",
                               label: MentionLink.basename(of: match.path),
                               detail: Self.parentPath(match.path),
                               payload: .path(match.path, isDir: match.isDir))
            }
        } catch {
            guard mine == generation else { return }
            items = []
            errorText = Self.pathError(error)
        }
        isLoading = false
    }

    private static func parentPath(_ path: String) -> String {
        let parts = path.split(separator: "/")
        guard parts.count > 1 else { return "" }
        return parts.dropLast().joined(separator: "/")
    }

    // MARK: Errors
    //
    // A failure must NEVER render as "no matching files": cross-device searches
    // fail for reasons the user can act on, and the empty state hid them
    // (crates/ui/src/composer.rs:3296-3300). These strings are verbatim from
    // the desktop (composer.rs:3311-3335).

    private static func pathError(_ error: Error) -> String {
        guard let relay = error as? RelayError else { return "File search failed" }
        if relay.isUnknownMethod("SearchFiles") {
            return "The session's device runs an older zeron — update it to search its files"
        }
        switch relay {
        case .notConnected, .hostOffline, .timeout:
            return "The session's device is unreachable"
        case .rpc:
            return "File search failed"
        }
    }

    private static func commandError(_ error: Error) -> String {
        guard let relay = error as? RelayError else { return "Couldn't load this agent's commands" }
        if relay.isUnknownMethod("ListCommands") {
            return "The session's device runs an older zeron — update it to list commands"
        }
        switch relay {
        case .notConnected, .hostOffline, .timeout:
            return "The session's device is unreachable"
        case .rpc:
            return "Couldn't load this agent's commands"
        }
    }
}
