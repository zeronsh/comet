import CryptoKit
import XCTest
@testable import Zeron

final class VaultChannelTests: XCTestCase {
    struct Fixture: Decodable {
        struct Frame: Decodable { let plaintext, outgoing, incoming: String }
        let first, second, third, peerKey: String
        let frames: [Frame]
    }
    private func fixture() throws -> Fixture {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        return try JSONDecoder().decode(Fixture.self, from: Data(contentsOf:
            root.appendingPathComponent("crates/crypto/tests/fixtures/channel.json")))
    }
    private func bytes(_ hex: String) -> Data {
        let raw = Array(hex.utf8)
        func nibble(_ byte: UInt8) -> UInt8 { byte <= 57 ? byte - 48 : byte - 87 }
        return Data(stride(from: 0, to: raw.count, by: 2).map {
            nibble(raw[$0]) * 16 + nibble(raw[$0 + 1])
        })
    }
    private func handshake(vault: UInt8 = 1) throws -> VaultChannelHandshake {
        try VaultChannelHandshake(deviceId: Data(repeating: 7, count: 16), staticKey: Data(repeating: 3, count: 32),
            vaultId: Data(repeating: vault, count: 16), generation: Data(repeating: 2, count: 16),
            ephemeralKey: .init(rawRepresentation: Data(repeating: 5, count: 32)))
    }
    func testSnowHandshakeAndChunkedTransportInBothDirections() throws {
        let f = try fixture()
        let hs = try handshake()
        XCTAssertEqual(hs.first, bytes(f.first))
        let (third, channel) = try hs.finish(bytes(f.second)) { id, key in
            id == Data(repeating: 8, count: 16) && key == self.bytes(f.peerKey)
        }
        XCTAssertEqual(third, bytes(f.third))
        for frame in f.frames {
            XCTAssertEqual(try channel.seal(bytes(frame.plaintext)), bytes(frame.outgoing))
            XCTAssertEqual(try channel.open(bytes(frame.incoming)), bytes(frame.plaintext))
        }
        XCTAssertThrowsError(try hs.finish(bytes(f.second)) { _, _ in true })
        XCTAssertThrowsError(try channel.open(bytes(f.frames[0].incoming)))
        XCTAssertThrowsError(try channel.seal(Data()))
    }
    func testForeignVaultUnapprovedPeerAndDamagedHandshakeFailClosed() throws {
        let f = try fixture()
        XCTAssertThrowsError(try handshake(vault: 9).finish(bytes(f.second)) { _, _ in true })
        XCTAssertThrowsError(try handshake().finish(bytes(f.second)) { _, _ in false })
        var damaged = bytes(f.second)
        damaged[70] ^= 1
        XCTAssertThrowsError(try handshake().finish(damaged) { _, _ in true })
        XCTAssertThrowsError(try handshake().finish(Data()) { _, _ in true })
    }
    func testMalformedTransportRetiresChannel() throws {
        let f = try fixture()
        for damage in [Data(), bytes(f.frames[0].incoming).dropLast(), bytes(f.frames[0].incoming) + Data([0])] {
            let (_, channel) = try handshake().finish(bytes(f.second)) { _, _ in true }
            XCTAssertThrowsError(try channel.open(Data(damage)))
            XCTAssertThrowsError(try channel.open(bytes(f.frames[0].incoming)))
        }
        let (_, channel) = try handshake().finish(bytes(f.second)) { _, _ in true }
        XCTAssertThrowsError(try channel.seal(Data(repeating: 0, count: VaultChannel.maximum + 1)))
    }
}
