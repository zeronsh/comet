import Foundation

/// One host-side checkout watch. Multiple chats may share this target.
struct ChangeRequestWatchKey: Hashable {
    var deviceId: String
    var cwd: String
}

/// Active, fully identified checkouts that need host-side PR resolution.
func desiredChangeRequestTargets(
    chats: [Chat],
    spaces: [Space],
    unsupportedDevices: Set<String>
) -> Set<ChangeRequestWatchKey> {
    Set(chats.compactMap { chat in
        guard !chat.archived,
              !unsupportedDevices.contains(chat.deviceId),
              let branch = chat.branch?.trimmingCharacters(in: .whitespacesAndNewlines),
              !branch.isEmpty,
              let cwd = effectiveChatCwd(chat, spaces: spaces) else { return nil }
        return ChangeRequestWatchKey(deviceId: chat.deviceId, cwd: cwd)
    })
}

/// Resolve a row snapshot without trusting the watch key alone. Branch and
/// canonical checkout identity prevent stale PRs flashing after a ref switch.
func resolvedChangeRequest(
    for chat: Chat,
    spaces: [Space],
    snapshots: [ChangeRequestWatchKey: CheckoutChangeRequestStatus]
) -> ChangeRequestSummary? {
    guard let branch = chat.branch?.trimmingCharacters(in: .whitespacesAndNewlines),
          !branch.isEmpty,
          let cwd = effectiveChatCwd(chat, spaces: spaces),
          let snapshot = snapshots[ChangeRequestWatchKey(deviceId: chat.deviceId, cwd: cwd)],
          snapshot.deviceId == chat.deviceId,
          snapshot.cwd == cwd,
          snapshot.branch == branch else { return nil }
    if let checkoutId = chat.checkoutId,
       (snapshot.checkoutId.isEmpty || snapshot.checkoutId != checkoutId) {
        return nil
    }
    return snapshot.changeRequest
}

func isUnknownChangeRequestMethod(_ error: Error) -> Bool {
    guard let relayError = error as? RelayError,
          case .rpc(let message) = relayError else { return false }
    return message.lowercased().hasPrefix("unknown method:")
}

private func effectiveChatCwd(_ chat: Chat, spaces: [Space]) -> String? {
    if let cwd = chat.cwd?.trimmingCharacters(in: .whitespacesAndNewlines), !cwd.isEmpty {
        return cwd
    }
    guard let spaceId = chat.spaceId,
          let path = spaces.first(where: {
              $0.id == spaceId && $0.deviceId == chat.deviceId
          })?.path.trimmingCharacters(in: .whitespacesAndNewlines),
          !path.isEmpty else { return nil }
    return path
}
