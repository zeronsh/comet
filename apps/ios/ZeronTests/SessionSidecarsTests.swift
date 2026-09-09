import XCTest
@testable import Zeron

final class SessionSidecarsTests: XCTestCase {
    private func config() -> AppConfig {
        AppConfig(edgeURL: URL(string: "http://localhost:27640")!, mode: .dev,
            userId: "test", orgId: "test", deviceId: "phone", deviceName: "Phone", devBearer: "test@test")
    }
    func testForeignAndEscapingBlobReferencesAreRefused() throws {
        for ref in ["other/part", "chat/../x", "chat/..", "chat/.", "chat/a?token=x", "chat/a%2fb", "chat/", "/part"] {
            XCTAssertThrowsError(try SessionSidecars.blobPart(ref: ref, chatId: "chat"))
        }
        XCTAssertEqual(try SessionSidecars.blobPart(ref: "chat/tool:1#2.diff", chatId: "chat"), "tool:1#2.diff")
    }
    func testBlockedProfileNeverRequestsSidecars() async {
        let reader = SessionSidecars(config: config(), chatId: "chat", encrypted: true, transport: { _ in
            XCTFail("Blocked profile must not dial")
            return (Data(), 200)
        })
        do { _ = try await reader.tail(); XCTFail("Expected access refusal") } catch {}
    }
    func testEncryptedReaderRefusesPlaintextAndLegacyRoute() async {
        let config = config()
        config.setSyncAccess(.encrypted)
        let reader = SessionSidecars(config: config, chatId: "chat", encrypted: true, transport: { request in
            XCTAssertEqual(request.url?.path, "/chat2/chat-e1/tail")
            XCTAssertNil(request.url?.query)
            return (Data(#"{"chatId":"chat","messages":[]}"#.utf8), 200)
        })
        do { _ = try await reader.tail(); XCTFail("Expected plaintext refusal") } catch {}
        let legacy = SessionSidecars(config: config, chatId: "chat", encrypted: false, transport: { _ in
            XCTFail("Enrolled profile must never read legacy sidecars")
            return (Data(), 200)
        })
        do { _ = try await legacy.tail(); XCTFail("Expected refusal") } catch {}
    }
    func testTailDecodeKeepsOutputRefsAndRejectsWrongChat() throws {
        let json = Data(#"{"chatId":"chat","schemaVersion":1,"totalMessages":1,"messages":[{"id":"m","role":"assistant","createdAt":1000,"deviceId":"host","parts":[{"id":"p","kind":"tool","call":{"kind":"exec","command":"pwd"},"resolved":true,"isError":false,"outputRef":"chat/p","diffRef":"chat/p.diff"}]}]}"#.utf8)
        let entries = try SessionStore.decodeTail(json, chatId: "chat")
        guard case .tool(_, let call, _, let resolved) = entries[0].parts[0] else { return XCTFail("Missing tool") }
        XCTAssertTrue(resolved)
        XCTAssertEqual(call.string("outputRef"), "chat/p")
        XCTAssertEqual(call.string("diffRef"), "chat/p.diff")
        XCTAssertThrowsError(try SessionStore.decodeTail(json, chatId: "other"))
    }
}
