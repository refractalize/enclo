import AppKit
import LocalAuthentication

let argv = CommandLine.arguments
guard argv.count >= 3 else {
    fputs("usage: enclo-secret-approve-macos <app-name> <secret> [<secret>...]\n", stderr)
    exit(2)
}

let appName = argv[1]
let secretArgs = argv.dropFirst(2)

func formatSecret(_ raw: String) -> String {
    guard let qIdx = raw.firstIndex(of: "?") else { return "• \(raw)" }
    let name = String(raw[..<qIdx])
    let query = String(raw[raw.index(after: qIdx)...])
    var lines = "• \(name)"
    for pair in query.split(separator: "&") {
        let parts = pair.split(separator: "=", maxSplits: 1)
        let key = parts.first.map(String.init) ?? ""
        let val = (parts.dropFirst().first.map(String.init) ?? "")
            .removingPercentEncoding ?? ""
        lines += "\n    \(key): \(val)"
    }
    return lines
}

let body = secretArgs.map(formatSecret).joined(separator: "\n\n")

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
app.activate(ignoringOtherApps: true)

let alert = NSAlert()
alert.messageText = "\(appName) wants to access secrets"
alert.informativeText = body
alert.addButton(withTitle: "Allow")
alert.addButton(withTitle: "Deny")
guard alert.runModal() == .alertFirstButtonReturn else { exit(1) }

let context = LAContext()
var nsError: NSError?
guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &nsError) else {
    fputs("enclo-secret-approve-macos: LocalAuthentication unavailable: \(nsError?.localizedDescription ?? "unknown")\n", stderr)
    exit(2)
}

let semaphore = DispatchSemaphore(value: 0)
var approved = false
context.evaluatePolicy(.deviceOwnerAuthentication,
                        localizedReason: "\(appName) secret access") { success, _ in
    approved = success
    semaphore.signal()
}
semaphore.wait()
exit(approved ? 0 : 1)
