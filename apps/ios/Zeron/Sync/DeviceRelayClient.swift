// Device-room relay RPC client — dials a device's room on the edge as a
// `client` peer and speaks ControlRpc to the HOST engine over a virtual
// socket (crates/rpc/src/device_room.rs + edge/src/device-room.ts).
//
// Frame codec (binary WS messages): uleb128(headerLen) ‖ headerJSON ‖ payload.
// Header key order MUST be {"s","k","to","from"} (byte parity with both
// implementations); clients never set `to`/`from` — the DO stamps `from`.
// RPC payloads are ndjson ControlRpc frames: {id, method, params} out,
// {id, ok|err|item|done} back. Relay control frames (kind " relay" — leading
// space is part of the constant) signal host_offline/host_closed.

import Foundation

enum RelayError: LocalizedError {
    case notConnected
    case hostOffline
    case rpc(String)
    case timeout

    var errorDescription: String? {
        switch self {
        case .notConnected: return "Not connected to the device"
        case .hostOffline: return "The device is offline"
        case .rpc(let message): return message
        case .timeout: return "The device didn't respond"
        }
    }
}

/// Routes multiplexed RPC replies without coupling the wire protocol to the
/// WebSocket lifecycle. Access is serialized by `DeviceRelayClient`'s actor.
final class DeviceRpcPending {
    typealias UnaryCompletion = (Result<Data, RelayError>) -> Void

    private struct PendingStream {
        let yield: (Data) throws -> Void
        let finish: (Error?) -> Void
    }

    private var unary: [UInt64: UnaryCompletion] = [:]
    private var streams: [UInt64: PendingStream] = [:]

    var unaryCount: Int { unary.count }
    var streamCount: Int { streams.count }

    /// Whether the request is still waiting — the timeout task checks this so
    /// a deadline firing AFTER the reply landed can't tear down a live link.
    func owns(id: UInt64) -> Bool {
        unary[id] != nil || streams[id] != nil
    }

    func registerUnary(id: UInt64, completion: @escaping UnaryCompletion) {
        unary[id] = completion
    }

    func registerStream<Response: Decodable>(
        id: UInt64,
        as _: Response.Type,
        onTermination: @escaping @Sendable () -> Void
    ) -> AsyncThrowingStream<Response, Error> {
        let pair = AsyncThrowingStream<Response, Error>.makeStream()
        streams[id] = PendingStream(
            yield: { data in
                pair.continuation.yield(try JSONDecoder().decode(Response.self, from: data))
            },
            finish: { error in
                if let error {
                    pair.continuation.finish(throwing: error)
                } else {
                    pair.continuation.finish()
                }
            }
        )
        pair.continuation.onTermination = { @Sendable _ in onTermination() }
        return pair.stream
    }

    func fail(id: UInt64, error: RelayError) {
        if let completion = unary.removeValue(forKey: id) {
            completion(.failure(error))
        } else if let stream = streams.removeValue(forKey: id) {
            stream.finish(error)
        }
    }

    func failAll(error: RelayError) {
        // Remove first: finishing a stream invokes onTermination, whose
        // cancellation task must observe that this request is already gone.
        let waitingUnary = unary
        let waitingStreams = streams
        unary.removeAll()
        streams.removeAll()
        for completion in waitingUnary.values {
            completion(.failure(error))
        }
        for stream in waitingStreams.values {
            stream.finish(error)
        }
    }

    /// Returns true only while the consumer still owns a live stream. This
    /// makes termination idempotent and prevents one consumer from touching
    /// another request sharing the socket.
    func removeStreamForCancellation(id: UInt64) -> Bool {
        streams.removeValue(forKey: id) != nil
    }

    @discardableResult
    func handlePayload(_ payload: Data) -> Int {
        guard let text = String(data: payload, encoding: .utf8) else { return 0 }
        var handled = 0
        for line in text.split(separator: "\n") {
            guard let object = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any],
                  route(object) else { continue }
            handled += 1
        }
        return handled
    }

    private func route(_ object: [String: Any]) -> Bool {
        guard let id = (object["id"] as? NSNumber)?.uint64Value else { return false }

        if let stream = streams[id] {
            if let message = object["err"] as? String {
                streams.removeValue(forKey: id)
                stream.finish(RelayError.rpc(message))
            } else if object.keys.contains("item") {
                do {
                    let data = try JSONSerialization.data(withJSONObject: object["item"] ?? NSNull(),
                                                          options: .fragmentsAllowed)
                    try stream.yield(data)
                } catch {
                    streams.removeValue(forKey: id)
                    stream.finish(error)
                }
            } else if (object["done"] as? Bool) == true {
                streams.removeValue(forKey: id)
                stream.finish(nil)
            } else if object.keys.contains("ok") {
                // Versioned streams may acknowledge readiness with `ok`
                // before producing items. The pending stream remains live.
            } else {
                streams.removeValue(forKey: id)
                stream.finish(RelayError.rpc("unexpected reply"))
            }
            return true
        }

        guard let completion = unary.removeValue(forKey: id) else { return false }
        if let message = object["err"] as? String {
            completion(.failure(.rpc(message)))
        } else if object.keys.contains("ok"),
                  let data = try? JSONSerialization.data(withJSONObject: object["ok"] ?? NSNull(),
                                                         options: .fragmentsAllowed) {
            completion(.success(data))
        } else {
            completion(.failure(.rpc("unexpected reply")))
        }
        return true
    }
}

/// Registry-presence verdict gating peer dials (workspace_host.rs
/// PeerLiveness, PR #168): a `dark` verdict fails peer calls fast with ZERO
/// dials; every ambiguity stays `unknown` so a rows-down/relay-up incident
/// shape can never park the relay.
enum PeerLiveness: Sendable {
    case live
    case dark
    case unknown
}

actor DeviceRelayClient {
    static let rpcKind = "rpc"
    static let relayKind = " relay"  // leading space is intentional
    /// End-to-end liveness frame (rpc/src/device_room.rs ECHO_KIND): the DO's
    /// auto-pong answers from the EDGE, so a healthy client↔edge leg can mask
    /// a dead edge↔host leg — the echo is answered by the HOST.
    static let echoKind = "echo"
    /// device_room.rs PING_INTERVAL (10s) / SILENCE_LEASE (25s — tolerates one
    /// lost pong at +10/+20) / ECHO_DEADLINE (20s of host-echo silence on an
    /// echo-capable host = zombie path → drop and redial).
    static let pingIntervalNs: UInt64 = 10_000_000_000
    static let silenceLeaseNs: UInt64 = 25_000_000_000
    static let echoDeadlineNs: UInt64 = 20_000_000_000

    private let deviceId: String
    private let config: AppConfig
    /// Presence-based dial gate, wired by the owning store (MainActor reads
    /// the registry mirror). nil = never park (unknown).
    private let liveness: (@MainActor @Sendable () -> PeerLiveness)?

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var nextId: UInt64 = 1
    private let pending = DeviceRpcPending()
    private var connected = false
    /// Transport clock — any inbound (the DO's auto-pong included) counts.
    private var lastInbound = DispatchTime.now()
    /// Host-proof clock — echo replies and inbound RPC frames only.
    private var lastHostProof = DispatchTime.now()
    /// The host has echoed at least once on this link (feature detection:
    /// old hosts keep transport-lease-only behavior).
    private var echoSeen = false

    init(deviceId: String, config: AppConfig,
         liveness: (@MainActor @Sendable () -> PeerLiveness)? = nil) {
        self.deviceId = deviceId
        self.config = config
        self.liveness = liveness
    }

    // MARK: Lifecycle

    private func connect() async throws {
        if connected, socket != nil { return }
        // Registry-dark dial parking: a device with positive stale-presence
        // evidence fails fast with zero dials (it was 3-dial bursts every
        // ~60s to a device offline for days). A live cached link above wins;
        // unknown/ambiguous verdicts never park.
        if let liveness {
            let verdict = await liveness()
            if case .dark = verdict {
                throw RelayError.hostOffline
            }
        }
        guard let token = await config.currentToken() else { throw RelayError.notConnected }
        var components = URLComponents(url: config.edgeURL.appending(path: "device/\(deviceId)/ws"),
                                       resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        components.queryItems = [
            URLQueryItem(name: "role", value: "client"),
            // A reconnect is a new relay peer. Reusing a connId can briefly
            // leave two tagged sockets in the hibernating DO and route the
            // host's response to the stale predecessor.
            URLQueryItem(name: "connId", value: UUID().uuidString.lowercased()),
            URLQueryItem(name: "token", value: token),
        ]
        let task = URLSession.shared.webSocketTask(with: components.url!)
        socket = task
        task.resume()
        connected = true
        lastInbound = .now()
        lastHostProof = .now()
        echoSeen = false

        receiveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                guard let sock = await self.socket else { return }
                do {
                    let message = try await sock.receive()
                    await self.handleInbound(message)
                } catch {
                    await self.teardown(error: .hostOffline)
                    return
                }
            }
        }
        pingTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: DeviceRelayClient.pingIntervalNs)
                guard let self else { return }
                await self.keepaliveTick()
            }
        }
        // One echo immediately on connect: feature detection + instant proof
        // the edge↔host leg is real, not just our own client↔edge leg.
        await sendEcho()
    }

    func close() {
        teardown(error: .notConnected)
    }

    private func teardown(error: RelayError) {
        receiveTask?.cancel()
        pingTask?.cancel()
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        connected = false
        pending.failAll(error: error)
    }

    /// Keepalive + liveness in one 10s tick: judge the transport lease and
    /// the host-echo deadline, then ride a text ping (DO transport lease) and
    /// an echo frame (host proof) out together.
    private func keepaliveTick() async {
        guard socket != nil else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        if now - lastInbound.uptimeNanoseconds > Self.silenceLeaseNs {
            roomLog.warning("relay \(self.deviceId, privacy: .public): socket silent past lease; dropping link")
            teardown(error: .hostOffline)
            return
        }
        // Echo enforcement is feature-detected: only a host that has echoed
        // at least once on this link is held to the 20s deadline. This is
        // what kills zombie paths where the DO's auto-pong keeps our leg
        // looking healthy while the host's leg is dead.
        if echoSeen, now - lastHostProof.uptimeNanoseconds > Self.echoDeadlineNs {
            roomLog.warning("relay \(self.deviceId, privacy: .public): host echo silent; dropping zombie link")
            teardown(error: .hostOffline)
            return
        }
        try? await socket?.send(.string("ping"))
        await sendEcho()
    }

    private func sendEcho() async {
        guard let socket else { return }
        let frame = Self.encodeFrame(header: #"{"s":"echo","k":"echo"}"#, payload: Data())
        try? await socket.send(.data(frame))
    }

    // MARK: RPC

    /// One unary ControlRpc call to the host engine. The default 10s deadline
    /// suits interactive calls (the engine itself caps folder listing at 6s);
    /// attachment uploads pass longer ones (first chunk 90s for a cold dial,
    /// commit 150s to outlast the cross-device assemble — desktop state.ts).
    func call<Response: Decodable>(method: String, params: [String: Any],
                                   timeoutSeconds: UInt64 = 10) async throws -> Response {
        for attempt in 0..<3 {
            do {
                return try await callOnce(method: method, params: params,
                                          timeoutSeconds: timeoutSeconds)
            } catch let error as RelayError {
                guard attempt < 2 else { throw error }
                switch error {
                case .hostOffline, .notConnected:
                    teardown(error: error)
                    try? await Task.sleep(nanoseconds: UInt64(attempt + 1) * 250_000_000)
                case .rpc, .timeout:
                    throw error
                }
            }
        }
        throw RelayError.notConnected
    }

    private func callOnce<Response: Decodable>(
        method: String,
        params: [String: Any],
        timeoutSeconds: UInt64 = 10
    ) async throws -> Response {
        try await connect()
        let id = nextId
        nextId += 1
        // Always send a params object — the engine's serde rejects a missing
        // field even when every param is optional (ListFolders home listing).
        let frame: [String: Any] = ["id": id, "method": method, "params": params]
        let payload = try JSONSerialization.data(withJSONObject: frame)
        let data = Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#, payload: payload)

        // Install the waiter before sending. URLSession's async send may yield
        // long enough for a fast host reply to reach handleInbound; registering
        // afterward loses that reply and turns a successful call into a timeout.
        let result: Result<Data, RelayError> = await withCheckedContinuation { continuation in
            pending.registerUnary(id: id) { result in
                continuation.resume(returning: result)
            }
            Task {
                await self.send(data, for: id)
            }
            Task {
                try? await Task.sleep(nanoseconds: timeoutSeconds * 1_000_000_000)
                self.timeoutCall(id: id)
            }
        }
        switch result {
        case .failure(let error): throw error
        case .success(let ok):
            return try JSONDecoder().decode(Response.self, from: ok)
        }
    }

    private func send(_ data: Data, for id: UInt64) async {
        guard let socket else {
            failRequest(id: id, error: .notConnected)
            return
        }
        do {
            try await socket.send(.data(data))
        } catch {
            failRequest(id: id, error: .notConnected)
            teardown(error: .notConnected)
        }
    }

    private func failRequest(id: UInt64, error: RelayError) {
        pending.fail(id: id, error: error)
    }

    private func timeoutCall(id: UInt64) {
        guard pending.owns(id: id) else { return }
        failRequest(id: id, error: .timeout)
        // A lost reply on a socket the DO keeps auto-ponging is a zombie
        // path (worktree-send hang, 2026-08-18): invalidate the link so the
        // next call re-dials instead of hanging on the same dead leg.
        roomLog.warning("relay \(self.deviceId, privacy: .public): call timed out; dropping link as suspect")
        teardown(error: .notConnected)
    }

    /// A typed streaming ControlRpc call. Items remain registered until a
    /// terminal `done`/`err`, socket close, or consumer cancellation frame.
    func stream<Response: Decodable>(
        method: String,
        params: [String: Any]
    ) async throws -> AsyncThrowingStream<Response, Error> {
        try await connect()
        let id = nextId
        nextId += 1
        let frame: [String: Any] = ["id": id, "method": method, "params": params]
        let payload = try JSONSerialization.data(withJSONObject: frame)
        let data = Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#, payload: payload)
        let result = pending.registerStream(id: id, as: Response.self) { [weak self] in
            Task { await self?.cancelStream(id: id) }
        }

        // Registration precedes send so an immediate host item cannot race
        // ahead of its continuation. AsyncThrowingStream buffers until read.
        await send(data, for: id)
        return result
    }

    private func cancelStream(id: UInt64) async {
        guard pending.removeStreamForCancellation(id: id), let socket else { return }
        let frame: [String: Any] = ["id": id, "cancel": true]
        guard let payload = try? JSONSerialization.data(withJSONObject: frame) else { return }
        let data = Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#, payload: payload)
        do {
            try await socket.send(.data(data))
        } catch {
            teardown(error: .notConnected)
        }
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message) {
        lastInbound = .now()
        switch message {
        case .string:
            return  // "pong" — transport lease refreshed; proves only OUR leg
        case .data(let data):
            guard let (header, payload) = Self.decodeFrame(data) else { return }
            switch header.k {
            case Self.rpcKind:
                // An inbound RPC frame comes from the host — proof enough.
                lastHostProof = .now()
                echoSeen = true
                handleRpcPayload(payload)
            case Self.echoKind:
                // The HOST echoed our keepalive back — the end-to-end path
                // (client → edge → host → edge → client) is alive.
                lastHostProof = .now()
                echoSeen = true
            case Self.relayKind:
                // {"error":"host_offline"|"host_closed"|...} — link down.
                teardown(error: .hostOffline)
            default:
                return
            }
        @unknown default:
            return
        }
    }

    private func handleRpcPayload(_ payload: Data) {
        // ndjson may interleave many unary and streaming request ids.
        pending.handlePayload(payload)
    }

    // MARK: Frame codec

    struct FrameHeader: Decodable {
        var s: String?
        var k: String?
        var to: String?
        var from: String?
    }

    static func encodeFrame(header: String, payload: Data) -> Data {
        let headerBytes = Data(header.utf8)
        var out = Data()
        var len = UInt64(headerBytes.count)
        repeat {
            var byte = UInt8(len & 0x7f)
            len >>= 7
            if len != 0 { byte |= 0x80 }
            out.append(byte)
        } while len != 0
        out.append(headerBytes)
        out.append(payload)
        return out
    }

    static func decodeFrame(_ data: Data) -> (FrameHeader, Data)? {
        var offset = 0
        var length: UInt64 = 0
        var shift: UInt64 = 0
        let bytes = [UInt8](data)
        while offset < bytes.count {
            let byte = bytes[offset]
            offset += 1
            length |= UInt64(byte & 0x7f) << shift
            if byte & 0x80 == 0 { break }
            shift += 7
            if shift > 28 { return nil }
        }
        guard offset + Int(length) <= bytes.count else { return nil }
        let headerData = Data(bytes[offset..<offset + Int(length)])
        guard let header = try? JSONDecoder().decode(FrameHeader.self, from: headerData) else { return nil }
        let payload = Data(bytes[(offset + Int(length))...])
        return (header, payload)
    }
}
