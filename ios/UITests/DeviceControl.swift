// Screenshot and tap the running app on a physical device — the iOS answer to
// `adb shell screencap` / `adb shell input tap`.
//
// Driven by env vars. xcodebuild forwards anything prefixed TEST_RUNNER_ to the
// runner process with the prefix stripped:
//
//   ship.sh shot                  → screenshot only
//   ship.sh tap 0.5 0.62          → tap normalized (x, y), then screenshot
//   ship.sh drive '0.5,0.4 | wait:10 | 0.5,0.8'
//                                 → several steps in ONE app session
//
// `drive` exists because `app.launch()` restarts the app, so state built up by
// one invocation is gone by the next. Anything stateful — walking a wizard —
// has to happen inside a single run.
//
// NO TYPING. Text entry cannot be automated against a Slint app on iOS. All
// four mechanisms were tried on device and every one silently delivered
// nothing while the test still passed: app.typeText, element.typeText,
// XCUIElement.tap on keyboard keys, and coordinate taps on keyboard keys.
// Cause: winit's iOS view implements only the three-method UIKeyInput subset
// (winit src/platform_impl/ios/view.rs — hasText/insertText:/deleteBackward),
// not UITextInput, and Slint does not expose text widgets to AccessKit at all
// (slint-ui/slint#2895, open since 2023). XCUITest has no text sink to write
// into. A human typing works fine — the software keyboard's insertText: path
// is unaffected. Type by hand, automate everything around it.
//
// ponytail: normalized coordinates, not points. They survive a device swap,
// and every caller so far is eyeballing a screenshot anyway. Switch to
// XCUICoordinate offsets in points if something needs pixel accuracy.

import XCTest

final class DeviceControl: XCTestCase {

    override func setUp() {
        super.setUp()
        // A failed tap should still yield a screenshot to look at.
        continueAfterFailure = true
    }

    func testCapture() throws {
        let app = XCUIApplication()
        // CHATTER_ATTACH=1 brings the ALREADY-RUNNING app to the front instead
        // of relaunching it. `launch()` restarts the process, which throws away
        // whatever the previous launch established — so it can't be used to
        // photograph state that a plain `devicectl process launch` set up.
        if ProcessInfo.processInfo.environment["CHATTER_ATTACH"] != nil {
            app.activate()
        } else {
            app.launch()
        }

        let env = ProcessInfo.processInfo.environment

        // Give the Slint/winit event loop a beat to draw its first frame.
        // Rust-side startup (SQLCipher open, keyring, services) runs before the
        // window appears, so the default is generous.
        let settle = Double(env["CHATTER_SETTLE"] ?? "") ?? 4.0
        Thread.sleep(forTimeInterval: settle)

        // CHATTER_DUMP=1 prints the accessibility tree XCUITest sees. Slint
        // publishes almost nothing, so this is mostly useful for confirming
        // that — but it is the first thing to check when a tap misses.
        if env["CHATTER_DUMP"] != nil {
            // Attachment, not print(): stdout from an on-device test runner
            // never reaches xcodebuild's console.
            let dump = XCTAttachment(string: app.debugDescription)
            dump.name = "element-tree"
            dump.lifetime = .keepAlways
            add(dump)
        }

        // `x,y` taps, `wait:N` sleeps. Steps are separated by `|`, and every one
        // gets a screenshot so a failure shows which step it stopped on.
        for (i, raw) in (env["CHATTER_STEPS"] ?? "").split(separator: "|").enumerated() {
            let step = raw.trimmingCharacters(in: .whitespaces)
            if step.isEmpty { continue }

            if step.hasPrefix("text:") {
                XCTFail("typing is not automatable on Slint/iOS — see the note at the top of this file. Type by hand, then drive taps.")
                return
            } else if let secs = step.dropFirstIfPrefixed("wait:").flatMap(Double.init) {
                // For steps that kick off slow work (registration does key
                // generation + several RPCs) — screenshot after it settles.
                Thread.sleep(forTimeInterval: secs)
            } else {
                let parts = step.split(separator: ",").compactMap { Double($0.trimmingCharacters(in: .whitespaces)) }
                guard parts.count == 2, (0...1).contains(parts[0]), (0...1).contains(parts[1]) else {
                    XCTFail("step must be `x,y` (0..1) or `wait:N` — got \(step)")
                    return
                }
                app.coordinate(withNormalizedOffset: CGVector(dx: parts[0], dy: parts[1])).tap()
                Thread.sleep(forTimeInterval: 1.5)
            }

            capture("step\(i + 1)")
        }

        capture("final")
    }

    private func capture(_ name: String) {
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}

private extension String {
    func dropFirstIfPrefixed(_ p: String) -> String? {
        hasPrefix(p) ? String(dropFirst(p.count)) : nil
    }
}
