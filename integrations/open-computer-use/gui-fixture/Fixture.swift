import AppKit
import Foundation

/// Only this test window is ever targeted by GUI acceptance. Its state file
/// witnesses real widget actions independently of the automation response.
@MainActor
final class Fixture: NSObject, NSApplicationDelegate {
    let state: URL
    let gate: URL
    var window: NSWindow!
    let message = NSTextField(string: "")
    let count = NSTextField(labelWithString: "Submissions: 0")
    var submissions = 0

    init(state: URL, gate: URL) {
        self.state = state
        self.gate = gate
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        window = NSWindow(contentRect: NSRect(x: 300, y: 300, width: 480, height: 230),
                          styleMask: [.titled, .closable], backing: .buffered, defer: false)
        window.title = "AtomCode CU Acceptance"
        let content = window.contentView!
        let label = NSTextField(labelWithString: "Acceptance message")
        label.frame = NSRect(x: 24, y: 178, width: 420, height: 24)
        content.addSubview(label)
        message.frame = NSRect(x: 24, y: 138, width: 420, height: 28)
        message.setAccessibilityLabel("Acceptance message")
        message.setAccessibilityIdentifier("acceptance-message")
        content.addSubview(message)
        for (title, selector, x) in [("Record once", #selector(record), 24), ("Gated record", #selector(gatedRecord), 244)] {
            let button = NSButton(title: title, target: self, action: selector)
            button.frame = NSRect(x: x, y: 82, width: 200, height: 34)
            button.setAccessibilityLabel(title)
            content.addSubview(button)
        }
        count.frame = NSRect(x: 24, y: 30, width: 420, height: 24)
        content.addSubview(count)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: false)
        save("ready")
    }

    @objc func record() {
        submissions += 1
        count.stringValue = "Submissions: \(submissions)"
        save("recorded")
    }

    @objc func gatedRecord() {
        save("entered-gated-action")
        // Bounded even if the test fails: never leave a user's App hung forever.
        let deadline = Date().addingTimeInterval(15)
        while !FileManager.default.fileExists(atPath: gate.path) && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.01)
        }
        if FileManager.default.fileExists(atPath: gate.path) {
            record()
        } else {
            save("gate-timed-out")
        }
    }

    func save(_ stage: String) {
        let value: [String: Any] = ["stage": stage, "submissions": submissions,
                                  "message": message.stringValue,
                                  "window_id": window.windowNumber,
                                  "process_id": Int(ProcessInfo.processInfo.processIdentifier)]
        do {
            let data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
            try data.write(to: state, options: .atomic)
        } catch {
            FileHandle.standardError.write(Data("fixture state write failed\n".utf8))
            NSApp.terminate(nil)
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

@main
struct Main {
    @MainActor static func main() {
        let args = CommandLine.arguments
        guard args.count == 5, args[1] == "--state-file", args[3] == "--release-gate",
              args[2].hasPrefix("/"), args[4].hasPrefix("/") else {
            FileHandle.standardError.write(Data("Use --state-file ABSOLUTE_PATH --release-gate ABSOLUTE_PATH\n".utf8))
            exit(2)
        }
        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        let fixture = Fixture(state: URL(fileURLWithPath: args[2]), gate: URL(fileURLWithPath: args[4]))
        app.delegate = fixture
        withExtendedLifetime(fixture) { app.run() }
    }
}
