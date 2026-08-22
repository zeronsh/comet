// The online/wake bus — a Swift port of crates/sync/src/wake.rs (PR #168's
// event-driven recovery). One process-wide bus turns "the network is back"
// into an event every parked backoff can select on, so recovery is
// event-driven, never timer luck:
//
//  - Every successful room join broadcasts (one recovered socket un-parks the
//    whole fleet — the desktop broadcasts on every successful WS dial).
//  - NWPathMonitor's offline→online transition broadcasts; while the path is
//    offline, waiters park on a coarse 30s safety recheck instead of burning
//    dials (the monitor may be wrong, so never full silence).
//  - The foreground health probe ({edge}/health, 3s) broadcasts on success so
//    a focus lands a redial in ~1 RTT.
//
// Waiters register a continuation at wait time, so only events that fire
// DURING the wait cut it short — the drain-stale-before-arming discipline the
// Rust side needs falls out of the design (your own last dial can't zero
// every future wait).

import Foundation

final class OnlineBus: @unchecked Sendable {
    static let shared = OnlineBus()

    /// While the OS path is offline, backoff waits park at least this long
    /// between rechecks (wake.rs OFFLINE_PARK_RECHECK).
    static let offlineParkRecheckMs = 30_000

    private let lock = NSLock()
    private var waiters: [UInt64: CheckedContinuation<Void, Never>] = [:]
    private var cancelledBeforeRegistration: Set<UInt64> = []
    private var nextWaiterId: UInt64 = 0
    private var pathOffline = false

    /// Empirical "the network is back": resumes every parked waiter.
    func notifyOnline() {
        let resumed = lock.withLock {
            let resumed = waiters
            waiters.removeAll()
            return resumed
        }
        for continuation in resumed.values {
            continuation.resume()
        }
    }

    /// OS path status from NWPathMonitor. Only a definitive "unsatisfied"
    /// parks; the offline→online transition broadcasts the online event.
    func setPathOnline(_ online: Bool) {
        let wasOffline = lock.withLock {
            let wasOffline = pathOffline
            pathOffline = !online
            return wasOffline
        }
        if online, wasOffline {
            notifyOnline()
        }
    }

    var isPathOffline: Bool {
        lock.withLock { pathOffline }
    }

    /// Sleep `ms`, cut short by an online event that fires during the wait.
    /// While the OS path reports offline the wait is stretched to at least
    /// the 30s park recheck — dials against a dead path are pure battery.
    func waitBackoff(ms: Int) async {
        var wait = ms
        if isPathOffline {
            wait = max(wait, Self.offlineParkRecheckMs)
        }
        let nanos = UInt64(wait) * 1_000_000
        await withTaskGroup(of: Void.self) { group in
            group.addTask {
                try? await Task.sleep(nanoseconds: nanos)
            }
            group.addTask { [self] in
                await nextOnlineEvent()
            }
            await group.next()
            group.cancelAll()
        }
    }

    /// Await the next online broadcast (cancellation-safe).
    private func nextOnlineEvent() async {
        let id = lock.withLock {
            nextWaiterId += 1
            return nextWaiterId
        }
        await withTaskCancellationHandler {
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                let resumeNow = lock.withLock {
                    if cancelledBeforeRegistration.remove(id) != nil {
                        return true
                    } else {
                        waiters[id] = continuation
                        return false
                    }
                }
                if resumeNow { continuation.resume() }
            }
        } onCancel: {
            let continuation = lock.withLock {
                let continuation = waiters.removeValue(forKey: id)
                if continuation == nil {
                    cancelledBeforeRegistration.insert(id)
                }
                return continuation
            }
            continuation?.resume()
        }
    }
}
