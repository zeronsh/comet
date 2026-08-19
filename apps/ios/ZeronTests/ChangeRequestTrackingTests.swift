import XCTest
@testable import Zeron

final class ChangeRequestTrackingTests: XCTestCase {
    private func chat(
        id: String,
        device: String,
        cwd: String? = "/repo",
        branch: String? = "feature/pr",
        checkout: String? = "checkout"
    ) -> Chat {
        Chat(id: id, deviceId: device, title: nil, archived: false, cwd: cwd,
             branch: branch, checkoutId: checkout, config: nil,
             lastMessagePreview: nil, lastMessageAt: nil, createdAt: 0,
             spaceId: "space", lastSeenAt: nil)
    }

    private func summary(state: ChangeRequestState = .open) -> ChangeRequestSummary {
        ChangeRequestSummary(provider: "github", number: 90, title: "Show host pull requests",
                             url: "https://github.com/acme/zeron/pull/90", state: state,
                             baseRef: "main", headRef: "feature/pr")
    }

    private func status(
        device: String = "local",
        cwd: String = "/repo",
        branch: String = "feature/pr",
        checkout: String = "checkout",
        changeRequest: ChangeRequestSummary? = nil
    ) -> CheckoutChangeRequestStatus {
        CheckoutChangeRequestStatus(checkoutId: checkout, deviceId: device, cwd: cwd,
                                    branch: branch, changeRequest: changeRequest,
                                    updatedAt: "2026-08-15T12:30:00Z")
    }

    func testContractDecodesEveryState() throws {
        for state in [ChangeRequestState.open, .merged, .closed] {
            let encoded = try JSONEncoder().encode(status(changeRequest: summary(state: state)))
            let decoded = try JSONDecoder().decode(CheckoutChangeRequestStatus.self, from: encoded)
            XCTAssertEqual(decoded.changeRequest?.state, state)
        }
    }

    func testLocalAndRemoteChatsDeduplicateByHostCheckoutPath() {
        let chats = [
            chat(id: "local-a", device: "local"),
            chat(id: "local-b", device: "local"),
            chat(id: "remote", device: "remote"),
        ]
        let targets = desiredChangeRequestTargets(chats: chats, spaces: [], unsupportedDevices: [])
        XCTAssertEqual(targets, [
            ChangeRequestWatchKey(deviceId: "local", cwd: "/repo"),
            ChangeRequestWatchKey(deviceId: "remote", cwd: "/repo"),
        ])
    }

    func testDemandRequiresActiveBranchAndFallsBackToOwningSpace() {
        var archived = chat(id: "archived", device: "local")
        archived.archived = true
        let noBranch = chat(id: "no-branch", device: "local", branch: nil)
        let fallback = chat(id: "fallback", device: "local", cwd: nil, checkout: nil)
        let wrongDeviceSpace = Space(id: "space", deviceId: "remote", path: "/wrong", name: nil,
                                     gitDetected: true, gitCheckedAt: nil, checkoutId: nil, createdAt: 0)
        let owningSpace = Space(id: "space", deviceId: "local", path: "/project", name: nil,
                                gitDetected: true, gitCheckedAt: nil, checkoutId: nil, createdAt: 0)

        XCTAssertEqual(
            desiredChangeRequestTargets(
                chats: [archived, noBranch, fallback], spaces: [wrongDeviceSpace, owningSpace],
                unsupportedDevices: []
            ),
            [ChangeRequestWatchKey(deviceId: "local", cwd: "/project")]
        )
        XCTAssertTrue(desiredChangeRequestTargets(
            chats: [fallback], spaces: [owningSpace], unsupportedDevices: ["local"]
        ).isEmpty)
    }

    func testSnapshotMustMatchHostBranchAndCheckout() {
        let target = ChangeRequestWatchKey(deviceId: "remote", cwd: "/repo")
        let targetChat = chat(id: "chat", device: "remote")
        var snapshots = [target: status(device: "remote", changeRequest: summary())]
        XCTAssertEqual(resolvedChangeRequest(for: targetChat, spaces: [], snapshots: snapshots)?.number, 90)

        snapshots[target]?.branch = "other"
        XCTAssertNil(resolvedChangeRequest(for: targetChat, spaces: [], snapshots: snapshots))
        snapshots[target] = status(device: "remote", checkout: "old", changeRequest: summary())
        XCTAssertNil(resolvedChangeRequest(for: targetChat, spaces: [], snapshots: snapshots))

        let legacyChat = chat(id: "legacy", device: "remote", checkout: nil)
        XCTAssertEqual(resolvedChangeRequest(for: legacyChat, spaces: [], snapshots: snapshots)?.number, 90)
    }

    func testSuccessfulNoneReplacesVisiblePullRequest() {
        let target = ChangeRequestWatchKey(deviceId: "local", cwd: "/repo")
        let targetChat = chat(id: "chat", device: "local")
        var snapshots = [target: status(changeRequest: summary())]
        XCTAssertNotNil(resolvedChangeRequest(for: targetChat, spaces: [], snapshots: snapshots))

        snapshots[target] = status(changeRequest: nil)
        XCTAssertNil(resolvedChangeRequest(for: targetChat, spaces: [], snapshots: snapshots))
    }

    func testUnknownMethodAndAccessibilityLabelsAreExplicit() {
        XCTAssertTrue(isUnknownChangeRequestMethod(RelayError.rpc(
            "unknown method: WatchCheckoutChangeRequest"
        )))
        XCTAssertFalse(isUnknownChangeRequestMethod(RelayError.hostOffline))
        XCTAssertEqual(summary(state: .merged).accessibilityLabel,
                       "Pull request 90, Merged, Show host pull requests")
    }
}
