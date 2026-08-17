// The pending-message queue on the session doc, from the phone's side.

import Loro
import XCTest
@testable import Zeron

@MainActor
final class MessageQueueTests: XCTestCase {
    private func store() -> SessionStore {
        SessionStore(chatId: "chat-1", config: AppConfig(
            edgeURL: URL(string: "https://example.test")!,
            mode: .dev, userId: "u1", orgId: "o1",
            deviceId: "ios-test", deviceName: "Dan’s iPhone",
            devBearer: "cmt_dev_test"))
    }

    private func texts(_ store: SessionStore) -> [String] {
        store.queue.map(\.text)
    }

    func testEnqueueStampsTheRowAndShowsItImmediately() {
        let store = store()
        XCTAssertTrue(store.queue.isEmpty)
        let id = store.enqueueMessage(text: "check the logs", attachments: ["uploads/a.png"])
        XCTAssertNotNil(id)
        XCTAssertEqual(store.queue.count, 1)
        let row = store.queue[0]
        XCTAssertEqual(row.text, "check the logs")
        XCTAssertEqual(row.attachments, ["uploads/a.png"])
        XCTAssertEqual(row.issuedBy, "ios-test")
        XCTAssertGreaterThan(row.issuedAt, 0)
        XCTAssertNil(row.editedAt)
        // Nothing to send is not a queue row.
        XCTAssertNil(store.enqueueMessage(text: "   "))
        XCTAssertEqual(store.queue.count, 1)
    }

    func testEditingToNothingDropsTheRow() {
        let store = store()
        guard let first = store.enqueueMessage(text: "first") else { return XCTFail("queued") }
        store.enqueueMessage(text: "second")

        store.updateQueued(id: first, text: "first, revised")
        XCTAssertEqual(texts(store), ["first, revised", "second"])
        XCTAssertNotNil(store.queue[0].editedAt)

        store.updateQueued(id: first, text: "  ")
        XCTAssertEqual(texts(store), ["second"])
        // A row that is already gone is a no-op, not a crash.
        store.updateQueued(id: first, text: "back?")
        XCTAssertEqual(texts(store), ["second"])
    }

    func testMovingByArrowsAndToAnIndex() {
        let store = store()
        let ids = ["a", "b", "c"].compactMap { store.enqueueMessage(text: $0) }
        XCTAssertEqual(ids.count, 3)

        store.moveQueued(id: ids[2], by: -1)
        XCTAssertEqual(texts(store), ["a", "c", "b"])
        // Already at the top: nothing moves.
        store.moveQueued(id: ids[0], by: -1)
        XCTAssertEqual(texts(store), ["a", "c", "b"])
        // A drop past the end lands at the back rather than failing.
        store.moveQueued(id: ids[0], to: 99)
        XCTAssertEqual(texts(store), ["c", "b", "a"])
    }

    func testRemoveTakesOneRow() {
        let store = store()
        let ids = ["a", "b"].compactMap { store.enqueueMessage(text: $0) }
        store.removeQueued(id: ids[0])
        XCTAssertEqual(texts(store), ["b"])
        store.removeQueued(id: ids[0])
        XCTAssertEqual(texts(store), ["b"])
    }

    /// The Mac's rows land here — same container, same field names.
    func testRowsWrittenByAnotherDeviceDecode() {
        let store = store()
        let list = store.doc.getMovableList(id: "queue")
        let map = try! list.insertMapContainer(pos: 0, child: LoroMap())
        try! map.insert(key: "id", v: "q-desktop")
        try! map.insert(key: "text", v: "from the Mac")
        try! map.insert(key: "issuedBy", v: "desktop")
        try! map.insert(key: "issuedAt", v: Int64(1_700_000_000_000))
        try! map.insert(key: "editedAt", v: Int64(1_700_000_000_500))
        store.doc.commit()
        store.refreshQueue()

        XCTAssertEqual(store.queue.count, 1)
        XCTAssertEqual(store.queue[0].id, "q-desktop")
        XCTAssertEqual(store.queue[0].issuedBy, "desktop")
        XCTAssertEqual(store.queue[0].editedAt, 1_700_000_000_500)
    }

    func testLabelsAndNeighbours() {
        XCTAssertNil(MessageQueue.label(0))
        XCTAssertEqual(MessageQueue.label(1), "1 queued")
        XCTAssertEqual(MessageQueue.label(4), "4 queued")
        XCTAssertEqual(MessageQueue.oneLine(" run the\n tests  now "), "run the tests now")
        XCTAssertEqual(MessageQueue.neighbour(of: 1, direction: -1, count: 3), 0)
        XCTAssertNil(MessageQueue.neighbour(of: 0, direction: -1, count: 3))
        XCTAssertNil(MessageQueue.neighbour(of: 2, direction: 1, count: 3))
    }
}
