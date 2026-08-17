// The session doc's pending-message queue, from the phone's side
// (crates/doc/src/queue.rs).
//
// Unlike `messages` — host-only — every device may write here, so these are
// plain doc edits rather than commands for the host to drain: enqueue, retype,
// reorder and drop all land straight on the `queue` MovableList and sync out
// with the next update. A MovableList because reordering is a real move op:
// two devices dragging at once converge on one order with every row intact,
// where a delete+insert list would duplicate or lose rows.
//
// The one thing the phone cannot do locally is *send*: taking a row and turning
// it into a run (interrupting the turn to do it) is the host's job, so
// `sendQueuedNow` asks the host over the relay and only removes the row when
// the host says it took it.

import Foundation
import Loro

extension SessionStore {
    // MARK: Read

    nonisolated static func queuedFrom(_ value: LoroValue) -> QueuedMessage? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue?.trimmingCharacters(in: .whitespaces), !id.isEmpty,
              let text = m["text"]?.stringValue,
              !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return nil }
        return QueuedMessage(
            id: id,
            text: text,
            attachments: (m["attachments"]?.listValue ?? []).compactMap(\.stringValue),
            issuedBy: m["issuedBy"]?.stringValue ?? "",
            issuedAt: m["issuedAt"]?.i64Value ?? 0,
            editedAt: m["editedAt"]?.i64Value
        )
    }

    // MARK: Write

    /// Park a message on the queue. The host decides where it goes from there —
    /// straight into the running turn, or the front of the next one.
    @discardableResult
    func enqueueMessage(text: String, attachments: [String] = []) -> String? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        let id = UUID().uuidString.lowercased()
        let list = doc.getMovableList(id: "queue")
        do {
            let map = try list.insertMapContainer(pos: list.len(), child: LoroMap())
            try map.insert(key: "id", v: id)
            try map.insert(key: "text", v: text)
            try map.insert(key: "issuedBy", v: deviceId)
            try map.insert(key: "issuedAt", v: nowMs())
            if !attachments.isEmpty {
                try map.insert(key: "attachments", v: LoroValue.fromJSON(attachments))
            }
            doc.commit()
        } catch {
            return nil
        }
        refreshQueue()
        nudgeHost()
        return id
    }

    /// Retype a queued message. Emptying it is how you delete one — a message
    /// edited down to nothing is a message you have decided not to send.
    func updateQueued(id: String, text: String) {
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            removeQueued(id: id)
            return
        }
        guard let map = queueRowMap(id: id) else { return }
        do {
            try map.insert(key: "text", v: text)
            try map.insert(key: "editedAt", v: nowMs())
            doc.commit()
        } catch { return }
        refreshQueue()
    }

    /// Move a row to `to`, clamped to the queue. Same slot is a no-op.
    func moveQueued(id: String, to: Int) {
        let list = doc.getMovableList(id: "queue")
        let count = Int(list.len())
        guard count > 0, let from = queueIndex(of: id, in: list) else { return }
        let target = min(max(to, 0), count - 1)
        guard from != target else { return }
        do {
            try list.mov(from: UInt32(from), to: UInt32(target))
            doc.commit()
        } catch { return }
        refreshQueue()
    }

    /// Nudge a row one slot up (-1) or down (+1).
    func moveQueued(id: String, by direction: Int) {
        guard let from = queue.firstIndex(where: { $0.id == id }),
              let to = MessageQueue.neighbour(of: from, direction: direction, count: queue.count)
        else { return }
        moveQueued(id: id, to: to)
    }

    func removeQueued(id: String) {
        let list = doc.getMovableList(id: "queue")
        guard let index = queueIndex(of: id, in: list) else { return }
        do {
            try list.delete(pos: UInt32(index), len: 1)
            doc.commit()
        } catch { return }
        refreshQueue()
    }

    /// Send one queued message now, stopping whatever the agent is doing to
    /// take it. Only the host can do that, so this asks it over the relay and
    /// leaves the row alone if the ask fails — a message that silently vanished
    /// without being sent is the one outcome worth avoiding here.
    @discardableResult
    func sendQueuedNow(id: String) async -> Bool {
        struct Reply: Decodable { var sent: Bool? }
        guard queue.contains(where: { $0.id == id }), let relay = hostRelayClient() else {
            return false
        }
        do {
            let reply: Reply = try await relay.call(
                method: "SendQueuedMessageNow",
                params: ["chatId": chatId, "id": id]
            )
            return reply.sent ?? true
        } catch {
            roomLog.warning(
                "chat2 \(self.chatId, privacy: .public): send-now failed for queued \(id, privacy: .public)"
            )
            return false
        }
    }

    // MARK: Plumbing

    private func queueIndex(of id: String, in list: LoroMovableList) -> Int? {
        (0..<Int(list.len())).first { i in
            list.get(index: UInt32(i))?.asLoroMap()?.get(key: "id")?.asValue()?.stringValue == id
        }
    }

    private func queueRowMap(id: String) -> LoroMap? {
        let list = doc.getMovableList(id: "queue")
        guard let index = queueIndex(of: id, in: list) else { return nil }
        return list.get(index: UInt32(index))?.asLoroMap()
    }
}
