import XCTest

@MainActor
final class AppshotUITests: XCTestCase {
    private func capture(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testRemoteAppshotsRemainReadableAndOpenOnPhone() {
        let app = XCUIApplication()
        app.launchArguments = ["-demo", "-route", "chat:chat-tabs", "-appshots"]
        XCUIDevice.shared.orientation = .portrait
        app.launch()
        let card = app.buttons["appshot-card-2:/demo/appshot-square.png"]
        XCTAssertTrue(card.waitForExistence(timeout: 10))
        XCTAssertFalse(app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS %@", "PRIVATE_OBSERVED_TEXT_MUST_STAY_HIDDEN")).firstMatch.exists)
        XCTAssertTrue(app.staticTexts["Check the narrow layout."].exists)
        capture("appshots-ios-portrait")
        let strip = app.scrollViews["appshot-attachments"]
        strip.swipeRight()
        strip.swipeRight()
        let first = app.buttons["appshot-card-0:/demo/appshot-wide.png"]
        XCTAssertTrue(first.isHittable)
        capture("appshots-ios-first-capture")
        first.tap()
        let close = app.buttons["Close image preview"]
        XCTAssertTrue(close.waitForExistence(timeout: 5))
        capture("appshots-ios-lightbox")
        close.tap()
        let images = app.buttons["Preview image, 2 more attachments"]
        XCTAssertTrue(images.waitForExistence(timeout: 5))
        images.tap()
        XCTAssertTrue(app.navigationBars["Queued images"].waitForExistence(timeout: 5))
        capture("appshots-ios-queue-gallery")
        app.buttons["Done"].tap()
        XCUIDevice.shared.orientation = .landscapeLeft
        XCTAssertTrue(images.waitForExistence(timeout: 5))
        capture("appshots-ios-landscape")
        XCUIDevice.shared.orientation = .portrait
        app.buttons["More queue actions"].firstMatch.tap()
        XCTAssertTrue(app.buttons["Move down"].waitForExistence(timeout: 5))
        capture("appshots-ios-queue-actions")
    }
}
