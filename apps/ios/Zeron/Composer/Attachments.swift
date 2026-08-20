// Attachments — the iOS port of crates/ui/src/attachments.rs (itself a port
// of zeron's use-attachments.ts / message-attachments.ts): composer staging,
// the chunked upload to the chat's host device, the plain-text attachment-ref
// transport that rides the prompt, the transcript read-back cache, and the
// full-size preview lightbox.
//
// The transport is TEXT: local paths appended to the prompt under an
// "Attached images (local files — open them to view):" trailer — that's what
// persists in the doc, what the agent reads, and what every client parses
// back out to render thumbnails. RunRequest.attachments additionally carries
// the paths so a harness can inline the bytes.

import Photos
import PhotosUI
import SwiftUI

// MARK: - Text transport (message-attachments.ts)

/// The body used for image-only sends.
let attachmentOnlyText = "See the attached image(s)."

/// use-attachments.ts `withAttachments`: plain local paths appended to the
/// text — the files are staged on the device that runs the agent.
func withAttachments(text: String, paths: [String]) -> String {
    guard !paths.isEmpty else { return text }
    let body = text.isEmpty ? attachmentOnlyText : text
    let refs = paths.map { "- \($0)" }.joined(separator: "\n")
    return "\(body)\n\nAttached images (local files — open them to view):\n\(refs)"
}

/// An attachment ref parsed back out of a user message's text.
struct UserImageAttachment: Identifiable, Hashable {
    let id: String
    let path: String
    let name: String
}

struct ParsedUserMessage {
    var text: String
    var attachments: [UserImageAttachment]
}

private func nameFromPath(_ path: String) -> String {
    let name = path.split(separator: "/").last.map(String.init) ?? ""
    return name.isEmpty ? "image" : name
}

/// message-attachments.ts `parseUserMessageImages`: split the visible prompt
/// from its attachment-ref trailer (case-insensitive marker, `- path` lines).
func parseUserMessageImages(_ content: String) -> ParsedUserMessage {
    let lines = content.components(separatedBy: "\n")
    var markerIx: Int?
    for (ix, raw) in lines.enumerated() where ix > 0 {
        let line = raw.trimmingCharacters(in: .whitespaces)
        if lines[ix - 1].trimmingCharacters(in: .whitespaces).isEmpty,
           line.lowercased().hasPrefix("attached images (local files"),
           line.hasSuffix("):") {
            markerIx = ix
            break
        }
    }
    guard let markerIx else {
        return ParsedUserMessage(text: content, attachments: [])
    }
    let attachments = lines[(markerIx + 1)...].compactMap { line -> String? in
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard trimmed.hasPrefix("- ") else { return nil }
        let path = String(trimmed.dropFirst(2)).trimmingCharacters(in: .whitespaces)
        return path.isEmpty ? nil : path
    }.enumerated().map { ix, path in
        UserImageAttachment(id: "\(ix):\(path)", path: path, name: nameFromPath(path))
    }
    guard !attachments.isEmpty else {
        return ParsedUserMessage(text: content, attachments: [])
    }
    let body = lines[..<(markerIx - 1)].joined(separator: "\n")
        .trimmingCharacters(in: .whitespacesAndNewlines)
    return ParsedUserMessage(text: body == attachmentOnlyText ? "" : body,
                             attachments: attachments)
}

// MARK: - Staging (use-attachments.ts intake)

/// use-attachments.ts `MAX_ATTACHMENT_BYTES`.
let maxAttachmentBytes = 24 * 1024 * 1024
/// Base64 chars per `UploadChunk` (attachments.rs UPLOAD_CHUNK_B64_CHARS,
/// PR #164): ≈510KB binary — sized to clear Cloudflare's 1MiB WS message cap
/// with envelope headroom, and % 4 == 0 so every slice decodes independently.
/// The old 60k (~45KB) sequential chunks made a 5MB photo 112 round trips.
let uploadChunkB64Chars = 680_000
/// Chunks in flight at once (attachments.rs UPLOAD_CONCURRENCY): order is
/// irrelevant — host `seq` slots are positional and idempotent.
let uploadConcurrency = 3

/// Whole-attachment deadline (attachments.rs attachment_deadline): a lawful
/// crawl of retrying chunks must still FAIL visibly rather than spin for
/// hours (the 2026-08-19 image-send incident class).
func attachmentDeadlineSeconds(chunkCount: Int) -> Int {
    min(120 + 15 * chunkCount, 900)
}

/// An image staged in the composer, before upload. Bytes are what uploads;
/// the decoded UIImage feeds thumbnails, the lightbox, and the post-send
/// cache seed.
struct StagedAttachment: Identifiable, Hashable {
    let id: String
    /// File name with a type-matching extension (agents sniff by extension).
    let name: String
    let data: Data
    let image: UIImage

    static func == (lhs: Self, rhs: Self) -> Bool { lhs.id == rhs.id }
    func hash(into hasher: inout Hasher) { hasher.combine(id) }

    /// Stage picked photo bytes: keep formats the engine's read-back jail
    /// serves (png/jpg/gif/webp) as-is; transcode everything else (HEIC
    /// camera shots, mainly) to JPEG.
    static func stage(data: Data) -> StagedAttachment? {
        var bytes = data
        var ext = sniffExtension(data)
        if ext == nil {
            guard let image = UIImage(data: data),
                  let jpeg = image.jpegData(compressionQuality: 0.9) else { return nil }
            bytes = jpeg
            ext = "jpg"
        }
        guard bytes.count <= maxAttachmentBytes, let ext,
              let image = UIImage(data: bytes) else { return nil }
        let id = UUID().uuidString.lowercased()
        return StagedAttachment(id: id, name: "photo-\(id.prefix(8)).\(ext)",
                                data: bytes, image: image)
    }

    /// Magic-byte sniff for the formats both ends support.
    private static func sniffExtension(_ data: Data) -> String? {
        guard data.count >= 12 else { return nil }
        let b = [UInt8](data.prefix(12))
        if b[0] == 0x89, b[1] == 0x50, b[2] == 0x4E, b[3] == 0x47 { return "png" }
        if b[0] == 0xFF, b[1] == 0xD8, b[2] == 0xFF { return "jpg" }
        if b[0] == 0x47, b[1] == 0x49, b[2] == 0x46, b[3] == 0x38 { return "gif" }
        if b[0] == 0x52, b[1] == 0x49, b[2] == 0x46, b[3] == 0x46,
           b[8] == 0x57, b[9] == 0x45, b[10] == 0x42, b[11] == 0x50 { return "webp" }
        return nil
    }
}

// MARK: - Upload (attachments.rs upload_attachment, PR #164 shape)

/// Concurrency-safe progress tally shared by the parallel chunk tasks.
private final class UploadTally: @unchecked Sendable {
    private let lock = NSLock()
    private var done = 0

    func add(_ bytes: Int) -> Int {
        lock.lock(); defer { lock.unlock() }
        done += bytes
        return done
    }
}

/// Chunked upload straight to the host device's relay room: base64 slices as
/// `UploadChunk {uploadId, seq, data}` — positional `seq` makes retries
/// idempotent — 3 in flight at once, then `UploadCommit {uploadId, fileName}`
/// → the durable absolute path on the host. The whole upload races a
/// chunk-count-scaled deadline, and per-chunk retries stagger by seq so a
/// window that failed together doesn't re-collide in lockstep.
///
/// The caller mints `uploadId`: on the queued flow the id is the persisted
/// `pending://` ref's identity, and retries must re-commit the same file.
/// Progress reports the fraction of committed binary bytes, clamped at 0.99 —
/// the last point belongs to the commit.
func uploadAttachmentChunked(relay: DeviceRelayClient, name: String, data: Data,
                             uploadId: String? = nil,
                             progress: (@MainActor @Sendable (Double) -> Void)? = nil) async throws -> String {
    struct OkReply: Decodable { var ok: Bool? }
    struct CommitReply: Decodable { var path: String }

    let id = uploadId ?? UUID().uuidString.lowercased()
    // Slice the BINARY at a % 3 == 0 boundary (b64 chars / 4 * 3): each
    // slice's independent base64 then concatenates to the whole file's.
    let chunkBytes = uploadChunkB64Chars / 4 * 3
    var ranges: [Range<Int>] = []
    var offset = 0
    // An empty file still sends one empty chunk (the commit needs the
    // uploadId staged); an exact multiple gets no trailing empty chunk.
    repeat {
        let end = min(offset + chunkBytes, data.count)
        ranges.append(offset..<end)
        offset = end
    } while offset < data.count

    let tally = UploadTally()
    let total = max(data.count, 1)

    @Sendable func pushChunk(seq: Int, range: Range<Int>) async throws {
        let slice = data.subdata(in: range).base64EncodedString()
        // The whole first window shares the cold-dial allowance, not just
        // seq 0 — its chunks all race the same handshake.
        let timeout: UInt64 = seq < uploadConcurrency ? 90 : 30
        var attempt = 0
        while true {
            do {
                let _: OkReply = try await relay.call(
                    method: "UploadChunk",
                    params: ["uploadId": id, "seq": seq, "data": slice],
                    timeoutSeconds: timeout)
                break
            } catch {
                attempt += 1
                guard attempt < 3 else { throw error }
                // Degraded uploads must narrate (silent crawls read as hangs).
                roomLog.warning("upload \(id, privacy: .public): chunk \(seq) attempt \(attempt) failed (\(error.localizedDescription, privacy: .public)); retrying")
                try? await Task.sleep(nanoseconds: UInt64(50 * attempt * (seq + 1)) * 1_000_000)
            }
        }
        let done = tally.add(range.count)
        if let progress {
            let fraction = min(Double(done) / Double(total), 0.99)
            await progress(fraction)
        }
    }

    let deadline = attachmentDeadlineSeconds(chunkCount: ranges.count)
    return try await withThrowingTaskGroup(of: String?.self) { race in
        race.addTask {
            // 3-wide sliding window over the chunk list, then the commit
            // (which outlasts the engine's assemble + best-effort mirror).
            try await withThrowingTaskGroup(of: Void.self) { group in
                var iterator = ranges.enumerated().makeIterator()
                var launched = 0
                while launched < uploadConcurrency, let (seq, range) = iterator.next() {
                    group.addTask { try await pushChunk(seq: seq, range: range) }
                    launched += 1
                }
                while try await group.next() != nil {
                    if let (seq, range) = iterator.next() {
                        group.addTask { try await pushChunk(seq: seq, range: range) }
                    }
                }
            }
            let reply: CommitReply = try await relay.call(
                method: "UploadCommit",
                params: ["uploadId": id, "fileName": name],
                timeoutSeconds: 150)
            return reply.path
        }
        race.addTask {
            try? await Task.sleep(nanoseconds: UInt64(deadline) * 1_000_000_000)
            return nil  // deadline expired
        }
        defer { race.cancelAll() }
        guard let path = try await race.next() ?? nil else {
            roomLog.error("upload \(id, privacy: .public): exceeded the \(deadline)s deadline; failing the send")
            throw RelayError.timeout
        }
        return path
    }
}

// MARK: - Transcript image cache (attachments.rs image cache)

/// Decoded transcript images keyed by `(deviceId, path)`, loaded over the
/// owning device's relay in 45KB base64 chunks, seeded locally after a send
/// so own bubbles never round-trip. Bounded by an encoded-byte LRU budget;
/// failed loads retry on the 2s→15s ladder.
@MainActor
@Observable
final class AttachmentImageCache {
    static let shared = AttachmentImageCache()

    enum Snapshot {
        case loading
        case loaded(name: String, image: UIImage)
        case error
    }

    private struct Key: Hashable {
        let deviceId: String
        let path: String
    }

    private enum Entry {
        case loading(attempts: Int)
        case loaded(name: String, image: UIImage, bytes: Int, lastUsed: UInt64)
        case error(attempts: Int, at: Date)
    }

    private static let budgetBytes = 64 * 1024 * 1024
    private static let maxReadChunks = 1_000

    private var entries: [Key: Entry] = [:]
    @ObservationIgnored private var tick: UInt64 = 0
    @ObservationIgnored private var loadedBytes = 0
    @ObservationIgnored private var config: AppConfig?
    @ObservationIgnored private var relays: [String: DeviceRelayClient] = [:]

    func configure(config: AppConfig) {
        if self.config !== config {
            self.config = config
            relays = [:]
        }
    }

    func snapshot(deviceId: String, path: String) -> Snapshot {
        switch entries[Key(deviceId: deviceId, path: path)] {
        case .loaded(let name, let image, _, _):
            return .loaded(name: name, image: image)
        case .error(let attempts, let at)
            where Date().timeIntervalSince(at) < Self.retryDelay(attempts):
            return .error
        case .error:
            return .loading  // ladder elapsed; the next load() attempt owns it
        case .loading, .none:
            return .loading
        }
    }

    /// Kick a load if this source isn't already loaded/loading (errored
    /// sources retry only after their backoff).
    func load(deviceId: String, path: String) {
        let key = Key(deviceId: deviceId, path: path)
        let attempts: Int
        switch entries[key] {
        case .loaded, .loading:
            return
        case .error(let n, let at):
            guard Date().timeIntervalSince(at) >= Self.retryDelay(n) else { return }
            attempts = n
        case .none:
            attempts = 0
        }
        guard let config else { return }
        entries[key] = .loading(attempts: attempts)
        let relay = relays[deviceId] ?? {
            let client = DeviceRelayClient(deviceId: deviceId, config: config)
            relays[deviceId] = client
            return client
        }()
        Task { @MainActor [weak self] in
            let loaded = await Self.readImage(relay: relay, path: path)
            guard let self else { return }
            if let loaded {
                self.store(key: key, name: loaded.name, image: loaded.image, bytes: loaded.bytes)
            } else {
                self.entries[key] = .error(attempts: attempts + 1, at: Date())
            }
        }
    }

    /// Seed after a successful upload (composer send path) so the just-sent
    /// bubble renders from local bytes instead of a round-trip.
    func seed(deviceId: String, path: String, name: String, data: Data) {
        guard let image = UIImage(data: data) else { return }
        store(key: Key(deviceId: deviceId, path: path), name: name, image: image, bytes: data.count)
    }

    private func store(key: Key, name: String, image: UIImage, bytes: Int) {
        tick += 1
        if case .loaded(_, _, let old, _)? = entries[key] {
            loadedBytes -= old
        }
        entries[key] = .loaded(name: name, image: image, bytes: bytes, lastUsed: tick)
        loadedBytes += bytes
        while loadedBytes > Self.budgetBytes {
            let oldest = entries.compactMap { entry -> (UInt64, Key, Int)? in
                guard entry.key != key,
                      case .loaded(_, _, let bytes, let used) = entry.value else { return nil }
                return (used, entry.key, bytes)
            }.min { $0.0 < $1.0 }
            guard let oldest else { break }
            entries.removeValue(forKey: oldest.1)
            loadedBytes -= oldest.2
        }
    }

    private static func retryDelay(_ attempts: Int) -> TimeInterval {
        min(Double(2 << min(max(attempts - 1, 0), 3)), 15)
    }

    /// `ReadAttachmentChunk` loop: 45KB base64 chunks until `done` (bounded,
    /// with a stuck-offset guard).
    private static func readImage(relay: DeviceRelayClient, path: String)
        async -> (name: String, image: UIImage, bytes: Int)? {
        struct Chunk: Decodable {
            var name: String
            var mimeType: String
            var data: String
            var nextOffset: UInt64
            var done: Bool
        }
        var name = ""
        var b64 = ""
        var offset: UInt64 = 0
        var done = false
        for _ in 0..<maxReadChunks {
            guard let chunk: Chunk = try? await relay.call(
                method: "ReadAttachmentChunk",
                params: ["path": path, "offset": offset],
                timeoutSeconds: 20) else { return nil }
            name = chunk.name
            b64 += chunk.data
            done = chunk.done
            if done { break }
            guard chunk.nextOffset > offset else { return nil }
            offset = chunk.nextOffset
        }
        guard done, let data = Data(base64Encoded: b64), let image = UIImage(data: data) else {
            return nil
        }
        return (name.isEmpty ? nameFromPath(path) : name, image, data.count)
    }
}

// MARK: - Composer staged strip (composer.rs render_attachment_strip: 56pt
// thumbs, 8pt gaps, wrapped)

struct AttachmentStripView: View {
    let attachments: [StagedAttachment]
    let remove: (String) -> Void

    @State private var preview: AttachmentPreview?

    var body: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 56, maximum: 56), spacing: 8)],
                  alignment: .leading, spacing: 8) {
            ForEach(attachments) { att in
                Button {
                    preview = AttachmentPreview(name: att.name, image: att.image)
                } label: {
                    Image(uiImage: att.image)
                        .resizable()
                        .aspectRatio(contentMode: .fill)
                        .frame(width: 56, height: 56)
                        .clipShape(RoundedRectangle(cornerRadius: 10))
                        .overlay(RoundedRectangle(cornerRadius: 10)
                            .strokeBorder(whiteAlpha(0.11), lineWidth: 1))
                }
                .buttonStyle(.plain)
                .overlay(alignment: .topTrailing) {
                    Button {
                        remove(att.id)
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 8, weight: .bold))
                            .foregroundStyle(Theme.text)
                            .frame(width: 18, height: 18)
                            .background(.black.opacity(0.65), in: Circle())
                            .overlay(Circle().strokeBorder(whiteAlpha(0.2), lineWidth: 1))
                    }
                    .buttonStyle(.plain)
                    .offset(x: 5, y: -5)
                }
            }
        }
        .fullScreenCover(item: $preview) { AttachmentLightbox(preview: $0) }
    }
}

// MARK: - Transcript thumbnails (transcript.rs render_user_attachments:
// 112×80 thumbs, right-aligned fixed-height strip above the bubble)

struct UserAttachmentsStrip: View {
    let deviceId: String
    let attachments: [UserImageAttachment]

    var body: some View {
        HStack(spacing: 8) {
            Spacer(minLength: 0)
            ForEach(attachments) { att in
                AttachmentThumbView(deviceId: deviceId, path: att.path)
            }
        }
        // Fixed height: load-state flips never shift the transcript.
        .frame(height: 80)
        .frame(maxWidth: .infinity, alignment: .trailing)
        .clipped()
    }
}

struct AttachmentThumbView: View {
    let deviceId: String
    let path: String

    private let cache = AttachmentImageCache.shared
    @State private var preview: AttachmentPreview?

    var body: some View {
        Group {
            switch cache.snapshot(deviceId: deviceId, path: path) {
            case .loaded(let name, let image):
                Button {
                    preview = AttachmentPreview(name: name, image: image)
                } label: {
                    Image(uiImage: image)
                        .resizable()
                        .aspectRatio(contentMode: .fill)
                        .frame(width: 112, height: 80)
                        .clipped()
                }
                .buttonStyle(.plain)
            case .loading:
                ProgressView()
                    .controlSize(.small)
                    .tint(Theme.textFaint)
                    .frame(width: 112, height: 80)
            case .error:
                // Tap retries once the backoff ladder allows it.
                Button {
                    cache.load(deviceId: deviceId, path: path)
                } label: {
                    Image(systemName: "photo.badge.exclamationmark")
                        .font(.system(size: 16))
                        .foregroundStyle(Theme.textFaint)
                        .frame(width: 112, height: 80)
                }
                .buttonStyle(.plain)
            }
        }
        .background(whiteAlpha(0.035))
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(whiteAlpha(0.11), lineWidth: 1))
        .task(id: "\(deviceId)|\(path)") {
            cache.load(deviceId: deviceId, path: path)
        }
        .fullScreenCover(item: $preview) { AttachmentLightbox(preview: $0) }
    }
}

// MARK: - Lightbox (attachments.rs lightbox: dim scrim, image ≤85vh/90vw,
// name under it, any tap closes)

struct AttachmentPreview: Identifiable {
    var id: String { name }
    let name: String
    let image: UIImage
}

struct AttachmentLightbox: View {
    let preview: AttachmentPreview
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        GeometryReader { geo in
            ZStack {
                Color.black.opacity(0.9).ignoresSafeArea()
                VStack(spacing: 12) {
                    Image(uiImage: preview.image)
                        .resizable()
                        .aspectRatio(contentMode: .fit)
                        .frame(maxWidth: geo.size.width * 0.9, maxHeight: geo.size.height * 0.85)
                        .clipShape(RoundedRectangle(cornerRadius: 6))
                    Text(preview.name)
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .contentShape(Rectangle())
            .onTapGesture { dismiss() }
        }
        .presentationBackground(.clear)
    }
}
