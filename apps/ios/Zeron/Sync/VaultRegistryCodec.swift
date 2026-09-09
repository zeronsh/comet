import Foundation

struct VaultRegistryCodec {
    struct Field: Codable {
        var kind: String
        var id: String
        var field: String
        var hlc: String
        var value: JSONValue
    }

    /// Row lifecycle proof (purpose registryLifecycle, RFC 0001 §9): a
    /// member's sealed statement that row `kind/id` was deleted at `hlc`.
    struct Lifecycle: Codable {
        var kind: String
        var id: String
        var op: String
        var hlc: String
    }

    let vault: MobileVault
    let client: MobileVaultClient
    let userId: String
    private var object: Data { MobileVault.objectId(kind: "registry", id: userId) }

    func seal(_ batch: RegistryPendingBatch) async throws -> RegistryPendingBatch {
        var sealed = batch
        for index in sealed.ops.indices {
            let op = sealed.ops[index]
            if op.op == .delete {
                let data = try JSONEncoder().encode(Lifecycle(kind: op.kind, id: op.id, op: "delete", hlc: op.hlc))
                let (bytes, _) = try await vault.seal(data, object: object, purpose: .registryLifecycle, maximum: 1024, client: client)
                sealed.ops[index].proof = .object(["e1": .string(bytes.base64EncodedString())])
                continue
            }
            guard let fields = op.set else { continue }
            var output: [String: JSONValue] = [:]
            for (name, value) in fields {
                let clock = op.clocks?[name] ?? op.hlc
                let data = try JSONEncoder().encode(Field(kind: op.kind, id: op.id, field: name, hlc: clock, value: value))
                let (bytes, _) = try await vault.seal(data, object: object, purpose: .registryField, maximum: 8 * 1024, client: client)
                output[name] = .object(["e1": .string(bytes.base64EncodedString())])
            }
            sealed.ops[index].set = output
        }
        return sealed
    }

    /// Open every field (and every tombstone proof). A tombstone without a
    /// proof for exactly its row and clock is dropped — never applied — so a
    /// relay cannot delete rows; a key that is not held yet still throws
    /// (the caller withholds the batch and holds its cursor).
    func open(_ rows: [RegistryRow]) async throws -> [RegistryRow] {
        var output: [RegistryRow] = []
        for var row in rows {
            if row.deleted {
                guard let hlc = row.delHlc, let envelope = row.delProof?.objectValue, envelope.count == 1,
                      let text = envelope["e1"]?.stringValue, text.utf8.count <= 4096,
                      let bytes = Data(base64Encoded: text) else { continue }
                let plaintext: Data
                do {
                    plaintext = try await vault.open(bytes, object: object, purpose: .registryLifecycle, maximum: 1024, client: client)
                } catch MobileVaultError.verification {
                    continue
                }
                guard let proof = try? JSONDecoder().decode(Lifecycle.self, from: plaintext),
                      proof.kind == row.kind, proof.id == row.id, proof.op == "delete", proof.hlc == hlc else { continue }
                row.fields = [:]
                row.clocks = [:]
                output.append(row)
                continue
            }
            var fields: [String: JSONValue] = [:]
            var clocks: [String: String] = [:]
            for (name, value) in row.fields {
                guard let envelope = value.objectValue, envelope.count == 1,
                      let text = envelope["e1"]?.stringValue, text.utf8.count <= 24 * 1024,
                      let bytes = Data(base64Encoded: text), let clock = row.clocks[name] else { throw MobileVaultError.verification }
                let plaintext = try await vault.open(bytes, object: object, purpose: .registryField, maximum: 8 * 1024, client: client)
                let field = try JSONDecoder().decode(Field.self, from: plaintext)
                guard field.kind == row.kind, field.id == row.id, field.field == name, field.hlc == clock else { throw MobileVaultError.verification }
                fields[name] = field.value
                clocks[name] = clock
            }
            row.fields = fields
            row.clocks = clocks
            output.append(row)
        }
        return output
    }

    /// Merge opened rows against the verified baseline: fields by clock,
    /// tombstones only when causally newer than the baseline, and a live
    /// row over a verified tombstone only with a field newer than it
    /// (RFC 0001 §9 — a relay can neither delete nor resurrect a row).
    static func merge(_ rows: [RegistryRow], into baseline: [String: [String: RegistryRow]]) -> [RegistryRow] {
        rows.compactMap { row in
            let previous = baseline[row.kind]?[row.id]
            if row.deleted {
                if let previous {
                    let newer = previous.deleted ? hlcNewer(row.delHlc ?? "", previous.delHlc)
                                                 : hlcNewer(row.delHlc ?? "", maxClock(previous))
                    guard newer else { return nil }
                }
                return row
            }
            var merged: RegistryRow
            if let previous, !previous.deleted {
                merged = previous
            } else {
                merged = RegistryRow(kind: row.kind, id: row.id, seq: row.seq, deleted: false,
                                     delHlc: previous?.delHlc, fields: [:], clocks: [:])
            }
            for (name, value) in row.fields {
                guard let clock = row.clocks[name], hlcNewer(clock, merged.clocks[name]) || previous == nil else { continue }
                if value.isNull { merged.fields.removeValue(forKey: name) }
                else { merged.fields[name] = value }
                merged.clocks[name] = clock
            }
            if let previous, previous.deleted, let gone = previous.delHlc,
               !merged.clocks.values.contains(where: { $0 > gone }) {
                return nil
            }
            merged.seq = row.seq
            return merged
        }
    }
}
