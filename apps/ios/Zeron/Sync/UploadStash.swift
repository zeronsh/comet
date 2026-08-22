// Durable staging for queued attachments (PR #168's "a send is a local
// write"): the bytes of every pending://-referenced attachment persist on
// device, keyed by uploadId, so the delivery escort can re-derive its
// transfers after a retry, a relaunch, or a tapped "retry" — the command in
// the doc names the ref; the stash holds the bytes it points at.
//
// Lives under DocDisk's directory so sign-out hygiene (wipeAll) covers it.
// Entries are deleted when the host commits the upload; anything older than
// the command TTL (24h) is swept on demand.

import Foundation

enum UploadStash {
    static let pendingRefPrefix = "pending://"

    static var directory: URL {
        let base = DocDisk.directory.appendingPathComponent("uploads", isDirectory: true)
        try? FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        return base
    }

    /// The persisted ref (uploads.rs PENDING_REF_PREFIX): the ORIGINAL file
    /// name rides the ref; both ends sanitize at resolution.
    static func pendingRef(uploadId: String, name: String) -> String {
        "\(pendingRefPrefix)\(uploadId)/\(name)"
    }

    /// Parse a pending ref back into (uploadId, fileName).
    static func parseRef(_ ref: String) -> (uploadId: String, name: String)? {
        guard ref.hasPrefix(pendingRefPrefix) else { return nil }
        let body = ref.dropFirst(pendingRefPrefix.count)
        guard let slash = body.firstIndex(of: "/") else { return nil }
        let id = String(body[..<slash])
        let name = String(body[body.index(after: slash)...])
        guard !id.isEmpty, !name.isEmpty else { return nil }
        return (id, name)
    }

    private static func url(uploadId: String) -> URL {
        let safe = uploadId.filter { $0.isLetter || $0.isNumber || $0 == "-" }
        return directory.appendingPathComponent("\(safe).bin")
    }

    static func save(uploadId: String, data: Data) {
        try? data.write(to: url(uploadId: uploadId), options: .atomic)
    }

    static func load(uploadId: String) -> Data? {
        try? Data(contentsOf: url(uploadId: uploadId))
    }

    static func delete(uploadId: String) {
        try? FileManager.default.removeItem(at: url(uploadId: uploadId))
    }

    /// Drop entries older than the command TTL — their commands have expired,
    /// so no escort will ever want the bytes again.
    static func sweep(ttlMs: Int64 = commandDefaultTtlMs) {
        let fm = FileManager.default
        guard let files = try? fm.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: [.contentModificationDateKey]) else { return }
        let cutoff = Date(timeIntervalSinceNow: -Double(ttlMs) / 1000)
        for file in files {
            let modified = (try? file.resourceValues(forKeys: [.contentModificationDateKey])
                .contentModificationDate) ?? .distantPast
            if modified < cutoff {
                try? fm.removeItem(at: file)
            }
        }
    }
}
