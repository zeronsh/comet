import Foundation
import XCTest
@testable import Zeron

/// Run scripts/test-ios-vault-live.sh. Uses the actual iOS vault, URLSession
/// relay and sidecar clients against a Rust host and local workerd DeviceRoom.
final class MobileVaultLiveTests: XCTestCase {
    private let directory = URL(fileURLWithPath: "/tmp/comet-mobile-e2e", isDirectory: true)
    struct Connection: Decodable { let edge, org, user, relay, fingerprint, chat: String }
    struct Echo: Decodable { let text: String }

    @MainActor
    func testEnrollmentRelaySidecarsReconnectAndRevocation() async throws {
        let file = directory.appendingPathComponent("connection.json")
        guard FileManager.default.fileExists(atPath: file.path),
              !FileManager.default.fileExists(atPath: directory.appendingPathComponent("done").path) else { throw XCTSkip("Start the opt-in mobile_test_host first") }
        let connection = try JSONDecoder().decode(Connection.self, from: Data(contentsOf: file))
        let config = AppConfig(edgeURL: URL(string: connection.edge)!, mode: .dev,
            userId: connection.user, orgId: connection.org, deviceId: UUID().uuidString,
            deviceName: "iOS live test", devBearer: "\(connection.user)@\(connection.org)")
        let fingerprint = try XCTUnwrap(Data(vaultHex: connection.fingerprint, count: 32))
        _ = await config.vault.refresh(client: config.vaultClient)
        try await config.vault.enroll(fingerprint: fingerprint, client: config.vaultClient)
        var ready = false
        for _ in 0..<40 {
            let status = await config.vault.refresh(client: config.vaultClient)
            if status.phase == .ready { ready = true; break }
            try await Task.sleep(for: .milliseconds(250))
        }
        XCTAssertTrue(ready, "Desktop must approve the actual phone identity")
        guard ready else { return }
        // The app starts approved, then must learn about removal on foreground.
        let model = AppModel()
        model.signInDev(edgeURL: config.edgeURL, userId: connection.user, orgId: connection.org)
        for _ in 0..<40 {
            if model.vaultStatus?.phase == .ready { break }
            try await Task.sleep(for: .milliseconds(250))
        }
        XCTAssertEqual(model.vaultStatus?.phase, .ready)
        XCTAssertFalse(model.requiresVaultApproval)
        defer { model.signOut() }
        config.setSyncAccess(.encrypted)
        let relay = DeviceRelayClient(deviceId: connection.relay, config: config)
        // Concurrent cold calls share one handshake and preserve transport nonce order.
        try await withThrowingTaskGroup(of: Void.self) { group in
            for i in 0..<8 {
                group.addTask {
                    let text = "encrypted RPC \(i)"
                    let reply: Echo = try await relay.call(method: "Echo", params: ["text": text])
                    XCTAssertEqual(reply.text, text)
                }
            }
            try await group.waitForAll()
        }
        let stream: AsyncThrowingStream<Echo, Error> = try await relay.stream(method: "EchoStream", params: [:])
        var streamed: [String] = []
        for try await item in stream { streamed.append(item.text) }
        XCTAssertEqual(streamed, ["stream 0", "stream 1", "stream 2"])
        let large = String(repeating: "private chunk ", count: 20_000)
        let reply: Echo = try await relay.call(method: "Echo", params: ["text": large])
        XCTAssertEqual(reply.text, large)
        let sidecars = SessionSidecars(config: config, chatId: connection.chat, encrypted: true)
        let tail = try await sidecars.tail()
        let entries = try SessionStore.decodeTail(tail, chatId: connection.chat)
        XCTAssertEqual(entries.count, 1)
        let output = try await sidecars.blob(ref: "\(connection.chat)/tool-1")
        XCTAssertEqual(output, "Full encrypted tool output")
        await relay.close()
        let reopened: Echo = try await relay.call(method: "Echo", params: ["text": "after reconnect"])
        XCTAssertEqual(reopened.text, "after reconnect")
        try Data().write(to: directory.appendingPathComponent("revoke"))
        for _ in 0..<40 {
            if FileManager.default.fileExists(atPath: directory.appendingPathComponent("revoked").path) { break }
            try await Task.sleep(for: .milliseconds(250))
        }
        do {
            let _: Echo = try await relay.call(method: "Echo", params: ["text": "revoked"])
            XCTFail("Revoked phone must not use the established channel")
        } catch { /* Both the established channel and redial fail closed. */ }
        let status = await config.vault.refresh(client: config.vaultClient)
        XCTAssertEqual(status.phase, .revoked)
        model.foregrounded()
        for _ in 0..<40 {
            if model.vaultStatus?.phase == .revoked { break }
            try await Task.sleep(for: .milliseconds(250))
        }
        XCTAssertEqual(model.vaultStatus?.phase, .revoked)
        XCTAssertTrue(model.requiresVaultApproval, "Removal must replace the reconnecting UI with reapproval")
        XCTAssertNil(model.workspace, "Revoked profiles must stop their sync stores")
        // Reapproval must use a fresh identity and leave the blocking screen
        // only once the desktop has admitted it and delivered its keys.
        try FileManager.default.removeItem(at: directory.appendingPathComponent("revoke"))
        try await model.enrollVault(fingerprintHex: connection.fingerprint)
        for _ in 0..<40 {
            model.refreshVault()
            if model.vaultStatus?.phase == .ready { break }
            try await Task.sleep(for: .milliseconds(250))
        }
        XCTAssertEqual(model.vaultStatus?.phase, .ready)
        XCTAssertFalse(model.requiresVaultApproval)
        XCTAssertNotNil(model.workspace)
        await relay.close()
        config.invalidate()
        try Data().write(to: directory.appendingPathComponent("done"))
    }
}
