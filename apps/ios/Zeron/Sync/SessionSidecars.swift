import Foundation

/// Sidecars are display-only. They never advance a room cursor or enter its CRDT.
struct SessionSidecars: Sendable {
    static let maximum = 4 * 1024 * 1024
    let config: AppConfig
    let chatId: String
    let encrypted: Bool
    var transport: @Sendable (URLRequest) async throws -> (Data, Int) = { request in
        let (stream, response) = try await URLSession.shared.bytes(for: request)
        guard let http = response as? HTTPURLResponse else { throw MobileVaultError.unavailable }
        guard response.expectedContentLength <= maximum else { throw MobileVaultError.oversized }
        var bytes = Data()
        for try await byte in stream {
            guard bytes.count < maximum else { throw MobileVaultError.oversized }
            bytes.append(byte)
        }
        return (bytes, http.statusCode)
    }

    func tail() async throws -> Data {
        let room = encrypted ? MobileVault.encryptedRoomId(chatId) : chatId
        return try await fetch(path: ["chat2", room, "tail"], purpose: .tail)
    }

    static func blobPart(ref: String, chatId: String) throws -> String {
        let parts = ref.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2, parts[0] == chatId, !parts[1].isEmpty, parts[1].utf8.count <= 200,
              parts[1] != ".", parts[1] != "..",
              parts[1].utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0)
                  || (97...122).contains($0) || Array("._:#~-".utf8).contains($0) }) else {
            throw MobileVaultError.verification
        }
        return String(parts[1])
    }

    func blob(ref: String) async throws -> String {
        let part = try Self.blobPart(ref: ref, chatId: chatId)
        let room = encrypted ? MobileVault.encryptedRoomId(chatId) : chatId
        let data: Data
        do { data = try await fetch(path: ["blob", room, part], purpose: .blob) }
        catch MobileVaultError.http(404) where encrypted {
            // Compatibility with earlier sealed blobs. fetch still requires
            // a valid encrypted record; plaintext is never displayed.
            data = try await fetch(path: ["blob", chatId, part], purpose: .blob)
        }
        guard let text = String(data: data, encoding: .utf8) else { throw MobileVaultError.verification }
        return text
    }

    private func fetch(path: [String], purpose: VaultContentPurpose) async throws -> Data {
        guard config.permitsSync(encrypted: encrypted), let token = await config.currentToken() else {
            throw MobileVaultError.unavailable
        }
        var url = config.edgeURL
        for part in path { url.append(component: part) }
        var request = URLRequest(url: url, timeoutInterval: 30)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        let (bytes, status) = try await transport(request)
        guard status == 200 else { throw MobileVaultError.http(status) }
        guard bytes.count <= Self.maximum else { throw MobileVaultError.oversized }
        guard config.permitsSync(encrypted: encrypted) else { throw MobileVaultError.unavailable }
        let opened: Data
        if encrypted {
            opened = try await config.vault.open(bytes, object: MobileVault.objectId(kind: "chat", id: chatId),
                purpose: purpose, maximum: Self.maximum - 1024, client: config.vaultClient)
        } else { opened = bytes }
        guard config.permitsSync(encrypted: encrypted) else { throw MobileVaultError.unavailable }
        return opened
    }
}
