import CryptoKit
import Foundation

/// Fixed Noise_XX_25519_AESGCM_SHA256 initiator, matching crypto/channel.rs.
/// No negotiation or plaintext fallback. All primitives are supplied by CryptoKit.
final class VaultChannelHandshake {
    private let identity: Curve25519.KeyAgreement.PrivateKey
    private let ephemeral: Curve25519.KeyAgreement.PrivateKey
    private let deviceId: Data
    private var hash: Data
    private var chainingKey: Data
    private var cipher: VaultChannelCipher?
    private var consumed = false
    let first: Data

    init(deviceId: Data, staticKey: Data, vaultId: Data, generation: Data,
         ephemeralKey: Curve25519.KeyAgreement.PrivateKey = .init()) throws {
        guard deviceId.count == 16, vaultId.count == 16, generation.count == 16 else {
            throw MobileVaultError.verification
        }
        self.deviceId = deviceId
        identity = try .init(rawRepresentation: staticKey)
        ephemeral = ephemeralKey
        let initial = Data(SHA256.hash(data: Data("Noise_XX_25519_AESGCM_SHA256".utf8)))
        // Protocol names shorter than HASHLEN are zero-padded, not hashed.
        let name = Data("Noise_XX_25519_AESGCM_SHA256".utf8)
        hash = name.count <= 32 ? name + Data(repeating: 0, count: 32 - name.count) : initial
        chainingKey = hash
        hash = Data(SHA256.hash(data: hash + Data("zeron/device-channel/v1\0".utf8) + vaultId + generation))
        first = ephemeral.publicKey.rawRepresentation
        hash = Data(SHA256.hash(data: hash + first))
        hash = Data(SHA256.hash(data: hash)) // empty first payload
    }

    func finish(_ second: Data, accept: (Data, Data) -> Bool) throws -> (Data, VaultChannel) {
        guard !consumed, second.count == 112 else { throw MobileVaultError.verification }
        consumed = true
        let remoteEphemeral = Data(second.prefix(32))
        mixHash(remoteEphemeral)
        try mixKey(ephemeral, remoteEphemeral)
        let remoteStatic = try decrypt(Data(second.dropFirst(32).prefix(48)))
        try mixKey(ephemeral, remoteStatic)
        let remoteId = try decrypt(Data(second.suffix(32)))
        guard remoteId.count == 16, accept(remoteId, remoteStatic) else { throw MobileVaultError.verification }
        var third = try encrypt(identity.publicKey.rawRepresentation)
        try mixKey(identity, remoteEphemeral)
        third.append(try encrypt(deviceId))
        let keys = Self.derive(chainingKey, Data())
        return (third, VaultChannel(sendKey: keys.0, receiveKey: keys.1,
                                    peerId: remoteId, peerKey: remoteStatic))
    }

    private func mixHash(_ bytes: Data) { hash = Data(SHA256.hash(data: hash + bytes)) }
    private func mixKey(_ local: Curve25519.KeyAgreement.PrivateKey, _ remote: Data) throws {
        let shared = try local.sharedSecretFromKeyAgreement(with: .init(rawRepresentation: remote))
        let keys = Self.derive(chainingKey, shared.withUnsafeBytes { Data($0) })
        chainingKey = keys.0
        cipher = VaultChannelCipher(key: keys.1)
    }
    private func decrypt(_ bytes: Data) throws -> Data {
        guard let cipher else { throw MobileVaultError.verification }
        let result = try cipher.open(bytes, aad: hash)
        mixHash(bytes)
        return result
    }
    private func encrypt(_ bytes: Data) throws -> Data {
        guard let cipher else { throw MobileVaultError.verification }
        let result = try cipher.seal(bytes, aad: hash)
        mixHash(result)
        return result
    }
    private static func derive(_ salt: Data, _ input: Data) -> (Data, Data) {
        let temp = Data(HMAC<SHA256>.authenticationCode(for: input, using: SymmetricKey(data: salt)))
        let first = Data(HMAC<SHA256>.authenticationCode(for: Data([1]), using: SymmetricKey(data: temp)))
        let second = Data(HMAC<SHA256>.authenticationCode(for: first + Data([2]), using: SymmetricKey(data: temp)))
        return (first, second)
    }
}

private final class VaultChannelCipher {
    private let key: SymmetricKey
    private var counter: UInt64 = 0
    init(key: Data) { self.key = SymmetricKey(data: key) }
    private func nonce() throws -> AES.GCM.Nonce {
        guard counter < 1 << 32 else { throw MobileVaultError.verification }
        var value = counter.bigEndian
        return try AES.GCM.Nonce(data: Data(repeating: 0, count: 4) + withUnsafeBytes(of: &value) { Data($0) })
    }
    func seal(_ bytes: Data, aad: Data = Data()) throws -> Data {
        let box = try AES.GCM.seal(bytes, using: key, nonce: nonce(), authenticating: aad)
        counter += 1
        return box.ciphertext + box.tag
    }
    func open(_ bytes: Data, aad: Data = Data()) throws -> Data {
        guard bytes.count >= 16 else { throw MobileVaultError.verification }
        let box = try AES.GCM.SealedBox(nonce: nonce(), ciphertext: bytes.dropLast(16), tag: bytes.suffix(16))
        let result = try AES.GCM.open(box, using: key, authenticating: aad)
        counter += 1
        return result
    }
}

/// Actor-owned ordered transport. Any failure permanently retires this channel.
final class VaultChannel {
    static let maximum = 8 * 1024 * 1024
    private static let chunkSize = 65518
    let peerId: Data
    let peerKey: Data
    private let sender: VaultChannelCipher
    private let receiver: VaultChannelCipher
    private var failed = false
    init(sendKey: Data, receiveKey: Data, peerId: Data, peerKey: Data) {
        sender = VaultChannelCipher(key: sendKey)
        receiver = VaultChannelCipher(key: receiveKey)
        self.peerId = peerId
        self.peerKey = peerKey
    }
    func seal(_ bytes: Data) throws -> Data {
        guard !failed, bytes.count <= Self.maximum else { failed = true; throw MobileVaultError.oversized }
        do {
            var result = Data()
            var offset = 0
            repeat {
                let end = min(offset + Self.chunkSize, bytes.count)
                let sealed = try sender.seal(Data([end < bytes.count ? 1 : 0]) + bytes.subdata(in: offset..<end))
                result.append(UInt8(sealed.count >> 8))
                result.append(UInt8(sealed.count & 255))
                result.append(sealed)
                offset = end
            } while offset < bytes.count
            return result
        } catch { failed = true; throw error }
    }
    func open(_ bytes: Data) throws -> Data {
        guard !failed, bytes.count <= Self.maximum + (Self.maximum / Self.chunkSize + 1) * 19 else {
            failed = true
            throw MobileVaultError.oversized
        }
        do {
            let bytes = [UInt8](bytes)
            var offset = 0
            var result = Data()
            while true {
                guard offset + 2 <= bytes.count else { throw MobileVaultError.verification }
                let count = Int(bytes[offset]) * 256 + Int(bytes[offset + 1])
                offset += 2
                guard count >= 17, offset + count <= bytes.count else { throw MobileVaultError.verification }
                let plaintext = try receiver.open(Data(bytes[offset..<offset + count]))
                offset += count
                result.append(plaintext.dropFirst())
                guard result.count <= Self.maximum else { throw MobileVaultError.oversized }
                if plaintext.first == 0 {
                    guard offset == bytes.count else { throw MobileVaultError.verification }
                    return result
                }
                guard plaintext.first == 1 else { throw MobileVaultError.verification }
            }
        } catch { failed = true; throw error }
    }
}
