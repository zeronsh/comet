// Connectivity truth — the iOS port of the engine's compute_connectivity +
// DegradeGrace (crates/engine/src/doc_host.rs, PRs #168/#170) and the UI's
// send-status derivations (crates/ui/src/state.rs).
//
// One graced stream feeds every consumer (home pill, composer notice, row
// Queued/Failed badges): a source must be RAW-degraded for 4s continuously
// before it reports degraded — sub-second blips (room joins on navigation,
// idle-link wakes) never flash the UI — while recovery reports instantly
// (hide-fast, show-slow). Sources are independent: the OS path, the registry
// room, and each open chat room keep their own timers.

import Foundation
import Observation

enum ConnectivityState: Equatable {
    /// The OS path is definitively down (graced): sends are saved locally.
    case offline
    /// The registry room is down (graced) while the path looks fine.
    case reconnecting
    case connected
}

/// A pending send's user-visible truth (state.rs send_pending/send_queued/
/// send_undelivered). `failed` wins over `queued` in the UI.
enum SendState {
    case sending
    case queued
    case failed
}

/// state.rs UNDELIVERED_GRACE_MS: a send unadopted past this is explicitly
/// Failed (with a retry affordance), never a silent forever-spinner.
let undeliveredGraceMs: Int64 = 120_000

@MainActor
@Observable
final class ConnectivityCenter {
    /// doc_host.rs DEGRADE_GRACE (PR #170).
    static let degradeGraceMs: Int64 = 4_000

    private(set) var state: ConnectivityState = .connected
    /// Chat ids whose room is graced-degraded (only rooms that have dialed).
    private(set) var degradedChats: Set<String> = []
    /// 1Hz pulse, bumped only while something is degraded or a send is
    /// pending — views showing elapsed-based send states read it to
    /// subscribe; a healthy idle app never repaints on it.
    private(set) var pulse: UInt64 = 0

    /// Raw-source providers, wired by AppModel (poll-based, mirroring the
    /// engine's 1s recompute over in-memory stats).
    @ObservationIgnored var registryConnected: (() -> Bool)?
    @ObservationIgnored var chatRooms: (() -> [(id: String, connected: Bool)])?
    @ObservationIgnored var hasPendingSends: (() -> Bool)?

    @ObservationIgnored private var pathOffline = false
    /// Source key → when it first sampled raw-degraded (ms). Absent = healthy.
    @ObservationIgnored private var degradedSince: [String: Int64] = [:]
    @ObservationIgnored private var tickTask: Task<Void, Never>?

    func start() {
        guard tickTask == nil else { return }
        tickTask = Task { @MainActor [weak self] in
            while !Task.isCancelled {
                self?.recompute()
                try? await Task.sleep(nanoseconds: 1_000_000_000)
            }
        }
    }

    func stop() {
        tickTask?.cancel()
        tickTask = nil
    }

    func setPathOffline(_ offline: Bool) {
        pathOffline = offline
        recompute()
    }

    /// DegradeGrace: report degraded only once the raw state has persisted
    /// ≥ 4s; one healthy sample clears instantly.
    private func graced(_ key: String, raw: Bool, now: Int64) -> Bool {
        guard raw else {
            degradedSince[key] = nil
            return false
        }
        guard let since = degradedSince[key] else {
            degradedSince[key] = now
            return false
        }
        return now - since >= Self.degradeGraceMs
    }

    private func recompute() {
        let now = nowMs()
        let offline = graced("os", raw: pathOffline, now: now)
        let registryDown = graced("registry", raw: !(registryConnected?() ?? true), now: now)

        var chats: Set<String> = []
        var liveKeys: Set<String> = ["os", "registry"]
        for room in chatRooms?() ?? [] {
            let key = "chat:\(room.id)"
            liveKeys.insert(key)
            if graced(key, raw: !room.connected, now: now) {
                chats.insert(room.id)
            }
        }
        // Drop timers for rooms that closed (doc_host.rs retain_chats).
        if degradedSince.count > liveKeys.count {
            degradedSince = degradedSince.filter { liveKeys.contains($0.key) }
        }

        let newState: ConnectivityState = offline ? .offline
            : registryDown ? .reconnecting
            : .connected
        // Publish only on change — an idle connected app never invalidates.
        if newState != state { state = newState }
        if chats != degradedChats { degradedChats = chats }
        if newState != .connected || !chats.isEmpty || !degradedSince.isEmpty
            || (hasPendingSends?() ?? false) {
            pulse &+= 1
        }
    }
}
