// Tests for the network-reliability port (desktop PRs #159/#164/#165/#168/
// #170): version gates, pending:// ref round-trips, the upload chunk plan's
// wire invariants, the whole-attachment deadline, and the relay echo frame.

import Loro
import XCTest
@testable import Zeron

final class NetworkReliabilityTests: XCTestCase {
    // MARK: versionTriple (proto version_triple port)

    func testVersionTripleParsesPlainAndSuffixed() {
        XCTAssertTrue(versionTriple("0.2.12")! == (0, 2, 12))
        XCTAssertTrue(versionTriple("1.10.3-beta.1")! == (1, 10, 3))
        XCTAssertTrue(versionTriple("0.2.12+build7")! == (0, 2, 12))
    }

    func testVersionTripleRejectsJunk() {
        XCTAssertNil(versionTriple(""))
        XCTAssertNil(versionTriple("0.2"))
        XCTAssertNil(versionTriple("a.b.c"))
        XCTAssertNil(versionTriple("0.2.x"))
        XCTAssertNil(versionTriple("0.2.12rc1"), "junk directly after the patch digits is not a version")
    }

    func testVersionGateComparison() {
        let min = (0, 2, 12)
        XCTAssertTrue(versionTriple("0.2.12")! >= min)
        XCTAssertTrue(versionTriple("0.3.0")! >= min)
        XCTAssertTrue(versionTriple("1.0.0")! >= min)
        XCTAssertFalse(versionTriple("0.2.11")! >= min)
    }

    // MARK: pending:// refs (uploads.rs PENDING_REF_PREFIX)

    func testPendingRefRoundTrip() {
        let ref = UploadStash.pendingRef(uploadId: "abc-123", name: "photo one.png")
        XCTAssertEqual(ref, "pending://abc-123/photo one.png")
        let parsed = UploadStash.parseRef(ref)
        XCTAssertEqual(parsed?.uploadId, "abc-123")
        XCTAssertEqual(parsed?.name, "photo one.png")
    }

    func testParseRefRejectsForeignAndMalformedRefs() {
        XCTAssertNil(UploadStash.parseRef("/Users/x/uploads/abc-photo.png"))
        XCTAssertNil(UploadStash.parseRef("pending://no-slash"))
        XCTAssertNil(UploadStash.parseRef("pending:///name.png"))
        XCTAssertNil(UploadStash.parseRef("pending://id/"))
    }

    func testStashSaveLoadDelete() {
        let id = "test-\(UUID().uuidString.lowercased())"
        let bytes = Data([1, 2, 3, 4])
        UploadStash.save(uploadId: id, data: bytes)
        XCTAssertEqual(UploadStash.load(uploadId: id), bytes)
        UploadStash.delete(uploadId: id)
        XCTAssertNil(UploadStash.load(uploadId: id))
    }

    // MARK: upload chunk plan (attachments.rs PR #164 invariants)

    func testChunkSizeWireInvariants() {
        // % 4 == 0: every base64 slice decodes independently.
        XCTAssertEqual(uploadChunkB64Chars % 4, 0)
        // The binary slice boundary is % 3 == 0, so per-slice base64
        // concatenates to the whole file's encoding.
        XCTAssertEqual((uploadChunkB64Chars / 4 * 3) % 3, 0)
        // Envelope headroom under Cloudflare's 1 MiB WS message cap.
        XCTAssertLessThan(uploadChunkB64Chars + 1_024, 1_048_576)
    }

    func testAttachmentDeadlineFormula() {
        XCTAssertEqual(attachmentDeadlineSeconds(chunkCount: 1), 135)
        XCTAssertEqual(attachmentDeadlineSeconds(chunkCount: 10), 270)
        XCTAssertEqual(attachmentDeadlineSeconds(chunkCount: 1_000), 900, "capped at 15 minutes")
    }

    // MARK: relay echo frame (device_room.rs ECHO_KIND)

    func testEchoFrameRoundTrip() {
        let frame = DeviceRelayClient.encodeFrame(header: #"{"s":"echo","k":"echo"}"#,
                                                  payload: Data())
        let decoded = DeviceRelayClient.decodeFrame(frame)
        XCTAssertEqual(decoded?.0.k, DeviceRelayClient.echoKind)
        XCTAssertEqual(decoded?.1.count, 0)
    }

    func testPendingOwnsTracksLifecycle() {
        let pending = DeviceRpcPending()
        XCTAssertFalse(pending.owns(id: 1))
        pending.registerUnary(id: 1) { _ in }
        XCTAssertTrue(pending.owns(id: 1))
        pending.fail(id: 1, error: .timeout)
        XCTAssertFalse(pending.owns(id: 1), "a resolved call must not read as pending — the timeout task keys link teardown on it")
    }

    // MARK: RunRequest wire shape (proto agent.rs, PR #159)

    func testWorktreeSpecOmittedWhenNil() throws {
        let request = RunRequest(prompt: "hi", cwd: "/repo")
        let json = String(data: try JSONEncoder().encode(request), encoding: .utf8)!
        XCTAssertFalse(json.contains("worktree"), "old hosts must see the legacy shape")
    }

    // MARK: retry re-issue (PR #172 phone half)

    @MainActor
    func testRetryReissuesADeadSendAttemptExactlyOnce() throws {
        let config = AppConfig(edgeURL: URL(string: "http://localhost:1")!, mode: .dev,
                               userId: "u", orgId: "o", deviceId: "phone",
                               deviceName: "phone", tokens: nil, devBearer: "u@o")
        let store = SessionStore(chatId: "c-reissue", config: config)
        let commands = store.doc.getList(id: "commands")
        let dead = try commands.pushContainer(child: LoroMap())
        try dead.insert(key: "id", v: "cmd-old")
        try dead.insert(key: "kind", v: "run")
        try dead.insert(key: "payload", v: LoroValue.fromJSON([
            "kind": "run",
            "request": ["prompt": "hi", "cwd": "/repo"],
            "messageId": "msg-1",
        ]))
        try dead.insert(key: "issuedBy", v: "phone")
        try dead.insert(key: "issuedAt", v: Int64(1_000))
        try dead.insert(key: "expiresAt", v: nowMs() + 60_000)
        try dead.insert(key: "status", v: "rejected")
        store.doc.commit()

        store.reissueDeadSends()

        let after = try XCTUnwrap(store.doc.getDeepValue().mapValue?["commands"]?.listValue)
        XCTAssertEqual(after.count, 2, "a rejected attempt whose message never landed re-issues")
        let fresh = try XCTUnwrap(after[1].mapValue)
        XCTAssertEqual(fresh["status"]?.stringValue, "pending")
        XCTAssertNotEqual(fresh["id"]?.stringValue, "cmd-old", "exactly-once is per command id — retry mints a fresh one")
        XCTAssertEqual(fresh["payload"]?.mapValue?["messageId"]?.stringValue, "msg-1",
                       "the message id survives so the host's user-entry pre-write dedupes")

        // A live pending attempt for the message makes a re-issue a duplicate.
        store.reissueDeadSends()
        let again = try XCTUnwrap(store.doc.getDeepValue().mapValue?["commands"]?.listValue)
        XCTAssertEqual(again.count, 2, "a second retry with a live attempt queued must not duplicate")
    }

    // MARK: RunRequest wire shape (proto agent.rs, PR #159)

    func testWorktreeSpecEncodesCamelCase() throws {
        var request = RunRequest(prompt: "hi", cwd: "/repo")
        request.worktree = WorktreeSpec(repoPath: "/repo", base: "HEAD")
        let data = try JSONEncoder().encode(request)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let worktree = try XCTUnwrap(object["worktree"] as? [String: Any])
        XCTAssertEqual(worktree["repoPath"] as? String, "/repo")
        XCTAssertEqual(worktree["base"] as? String, "HEAD")
    }
}
