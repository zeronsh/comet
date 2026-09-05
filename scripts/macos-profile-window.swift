// Foreground/window validation for the native resource replay. Fails closed
// on a locked display so suspended rendering cannot look like a CPU win.
import Cocoa
import ApplicationServices

func fail(_ message: String) -> Never {
    fputs("\(message)\n", stderr)
    exit(1)
}

let session = CGSessionCopyCurrentDictionary() as? [String: Any] ?? [:]
if session["CGSSessionScreenIsLocked"] as? Bool == true {
    fail("Unlock the Mac before profiling native window rendering")
}
if CGDisplayIsAsleep(CGMainDisplayID()) != 0 {
    fail("Wake the display before profiling native window rendering")
}
if CommandLine.arguments.count == 1 { exit(0) }
guard let pid = Int32(CommandLine.arguments[1]),
      let app = NSRunningApplication(processIdentifier: pid) else { fail("Missing app process") }
if CommandLine.arguments.dropFirst(2).first == "--check" {
    guard app.isActive else { fail("Profiled app is no longer foreground; discard this run") }
    let rows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    guard rows.contains(where: { ($0[kCGWindowOwnerPID as String] as? Int32) == pid
        && ($0[kCGWindowLayer as String] as? Int) == 0
        && ($0[kCGWindowAlpha as String] as? Double ?? 0) > 0 }) else {
        fail("Profiled window is not onscreen; discard this run")
    }
    exit(0)
}
guard AXIsProcessTrusted() else { fail("Window profiling requires existing Accessibility access") }
app.activate(options: [.activateAllWindows])
let element = AXUIElementCreateApplication(pid)
var value: CFTypeRef?
guard AXUIElementCopyAttributeValue(element, kAXWindowsAttribute as CFString, &value) == .success,
      let windows = value as? [AXUIElement], windows.count == 1 else { fail("Expected one app window") }
let window = windows[0]
var size = CGSize(width: 1320, height: 880)
guard let sizeValue = AXValueCreate(.cgSize, &size),
      AXUIElementSetAttributeValue(window, kAXSizeAttribute as CFString, sizeValue) == .success,
      AXUIElementPerformAction(window, kAXRaiseAction as CFString) == .success else {
    fail("Could not size and raise the profiled window")
}
