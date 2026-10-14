// Native, isolated slow-consumer reproduction. Uses named pasteboards only;
// never touches the user's general clipboard or sends keyboard events.
// swiftc scripts/test-macos-paste.swift -o /tmp/handy-paste-repro
// /tmp/handy-paste-repro
import AppKit

let oldText = "HANDY_SYNTHETIC_OLD"
let newText = "HANDY_SYNTHETIC_NEW"
let transient = NSPasteboard.PasteboardType("org.nspasteboard.TransientType")

if CommandLine.arguments.count == 4, CommandLine.arguments[1] == "read" {
    Thread.sleep(forTimeInterval: Double(CommandLine.arguments[3])! / 1000)
    let board = NSPasteboard(name: .init(CommandLine.arguments[2]))
    let value = board.string(forType: .string)
    print(value == newText ? "transcript" : value == oldText ? "stale" : "missing")
    exit(0)
}

final class Provider: NSObject {
    var deliveredAt: Date?
    var calls = 0
    @objc func pasteboard(_ board: NSPasteboard, provideDataForType type: NSPasteboard.PasteboardType) {
        if type == .string {
            if board.setString(newText, forType: type) {
                calls += 1
                deliveredAt = Date()
            }
        } else {
            board.setString("", forType: type)
        }
    }
}

var failures = 0
for receiptBased in [false, true] {
    for delay in [50, 100, 300, 1000] {
        let name = "HandyPasteRepro-\(UUID().uuidString)"
        let board = NSPasteboard(name: .init(name))
        board.clearContents()
        board.setString(oldText, forType: .string)
        let provider = Provider()
        if receiptBased {
            board.declareTypes([.string, transient], owner: provider)
        } else {
            board.clearContents()
            board.setString(newText, forType: .string)
        }
        let publishedCount = board.changeCount
        let child = Process()
        child.executableURL = URL(fileURLWithPath: CommandLine.arguments[0])
        child.arguments = ["read", name, String(delay)]
        let output = Pipe()
        child.standardOutput = output
        try child.run()
        let start = Date()
        var restored = false
        while child.isRunning || (receiptBased && !restored && Date().timeIntervalSince(start) < 2) {
            RunLoop.current.run(until: Date().addingTimeInterval(0.005))
            let ready = receiptBased
                ? provider.deliveredAt.map { Date().timeIntervalSince($0) >= 0.2 } ?? false
                : Date().timeIntervalSince(start) >= 0.16 // 100ms chord + 60ms restoration delay
            if ready && !restored && board.changeCount == publishedCount {
                board.clearContents()
                board.setString(oldText, forType: .string)
                restored = true
            }
        }
        child.waitUntilExit()
        let result = String(data: output.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8)!
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let expected = receiptBased || delay < 160 ? "transcript" : "stale"
        if result != expected { failures += 1 }
        print("\(receiptBased ? "receipt" : "legacy") delay=\(delay)ms result=\(result) expected=\(expected) restored=\(restored)")
        board.releaseGlobally()
    }
}
exit(failures == 0 ? 0 : 1)
