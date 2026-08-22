import XCTest
@testable import Zeron

private struct RpcValue: Decodable, Equatable {
    let value: Int
}

private func rpcPayload(_ lines: String...) -> Data {
    Data(lines.joined(separator: "\n").utf8)
}

final class DeviceRelayClientTests: XCTestCase {
    func testUnaryStillResolvesOk() throws {
        let pending = DeviceRpcPending()
        var reply: Result<Data, RelayError>?
        pending.registerUnary(id: 1) { reply = $0 }

        XCTAssertEqual(pending.handlePayload(rpcPayload(#"{"id":1,"ok":{"value":7}}"#)), 1)
        let data = try XCTUnwrap(try reply?.get())
        XCTAssertEqual(try JSONDecoder().decode(RpcValue.self, from: data), RpcValue(value: 7))
        XCTAssertEqual(pending.unaryCount, 0)
    }

    func testStreamDeliversItemsInOrderAndItemKeepsItPending() async throws {
        let pending = DeviceRpcPending()
        let stream: AsyncThrowingStream<RpcValue, Error> = pending.registerStream(
            id: 2, as: RpcValue.self, onTermination: {}
        )

        XCTAssertEqual(pending.handlePayload(rpcPayload(#"{"id":2,"ok":{"stream":true}}"#)), 1)
        XCTAssertEqual(pending.streamCount, 1)
        XCTAssertEqual(pending.handlePayload(rpcPayload(#"{"id":2,"item":{"value":1}}"#)), 1)
        XCTAssertEqual(pending.streamCount, 1, "an item must not consume the pending stream")
        XCTAssertEqual(pending.handlePayload(rpcPayload(
            #"{"id":2,"item":{"value":2}}"#,
            #"{"id":2,"done":true}"#,
            #"{"id":2,"done":true}"#
        )), 2, "the second terminal frame must be ignored")

        var values: [RpcValue] = []
        for try await value in stream { values.append(value) }
        XCTAssertEqual(values, [RpcValue(value: 1), RpcValue(value: 2)])
        XCTAssertEqual(pending.streamCount, 0)
    }

    func testStreamErrFinishesWithRpcError() async {
        let pending = DeviceRpcPending()
        let stream: AsyncThrowingStream<RpcValue, Error> = pending.registerStream(
            id: 3, as: RpcValue.self, onTermination: {}
        )
        pending.handlePayload(rpcPayload(#"{"id":3,"err":"unknown method: WatchThing"}"#))

        do {
            for try await _ in stream {}
            XCTFail("expected stream error")
        } catch RelayError.rpc(let message) {
            XCTAssertEqual(message, "unknown method: WatchThing")
        } catch {
            XCTFail("unexpected error: \(error)")
        }
    }

    func testSocketCloseFailsEveryPendingRequest() async {
        let pending = DeviceRpcPending()
        var unaryError: RelayError?
        pending.registerUnary(id: 4) { result in
            if case .failure(let error) = result { unaryError = error }
        }
        let stream: AsyncThrowingStream<RpcValue, Error> = pending.registerStream(
            id: 5, as: RpcValue.self, onTermination: {}
        )

        pending.failAll(error: .hostOffline)

        if case .hostOffline = unaryError {} else { XCTFail("unary request was not failed") }
        do {
            for try await _ in stream {}
            XCTFail("expected stream error")
        } catch RelayError.hostOffline {
            // Expected.
        } catch {
            XCTFail("unexpected error: \(error)")
        }
        XCTAssertEqual(pending.unaryCount, 0)
        XCTAssertEqual(pending.streamCount, 0)
    }

    func testCancellingConsumerLeavesOtherRequestsAlone() async throws {
        let pending = DeviceRpcPending()
        let cancelled = expectation(description: "stream termination callback")
        let first: AsyncThrowingStream<RpcValue, Error> = pending.registerStream(id: 6, as: RpcValue.self) {
            XCTAssertTrue(pending.removeStreamForCancellation(id: 6))
            cancelled.fulfill()
        }
        let second: AsyncThrowingStream<RpcValue, Error> = pending.registerStream(
            id: 7, as: RpcValue.self, onTermination: {}
        )
        let waiting = Task {
            for try await _ in first {}
        }

        waiting.cancel()
        await fulfillment(of: [cancelled], timeout: 1)
        _ = await waiting.result
        XCTAssertEqual(pending.streamCount, 1)

        pending.handlePayload(rpcPayload(
            #"{"id":7,"item":{"value":9}}"#,
            #"{"id":7,"done":true}"#
        ))
        var values: [RpcValue] = []
        for try await value in second { values.append(value) }
        XCTAssertEqual(values, [RpcValue(value: 9)])
    }

    func testInterleavedFramesRouteByRequestId() async throws {
        let pending = DeviceRpcPending()
        var unary: Result<Data, RelayError>?
        pending.registerUnary(id: 8) { unary = $0 }
        let first: AsyncThrowingStream<RpcValue, Error> = pending.registerStream(
            id: 9, as: RpcValue.self, onTermination: {}
        )
        let second: AsyncThrowingStream<RpcValue, Error> = pending.registerStream(
            id: 10, as: RpcValue.self, onTermination: {}
        )

        XCTAssertEqual(pending.handlePayload(rpcPayload(
            #"{"id":9,"item":{"value":1}}"#,
            #"{"id":8,"ok":{"value":8}}"#,
            #"{"id":10,"item":{"value":2}}"#,
            #"{"id":9,"item":{"value":3}}"#,
            #"{"id":10,"done":true}"#,
            #"{"id":9,"done":true}"#
        )), 6)

        let unaryData = try XCTUnwrap(try unary?.get())
        XCTAssertEqual(try JSONDecoder().decode(RpcValue.self, from: unaryData), RpcValue(value: 8))
        var firstValues: [RpcValue] = []
        for try await value in first { firstValues.append(value) }
        var secondValues: [RpcValue] = []
        for try await value in second { secondValues.append(value) }
        XCTAssertEqual(firstValues, [RpcValue(value: 1), RpcValue(value: 3)])
        XCTAssertEqual(secondValues, [RpcValue(value: 2)])
    }
}
