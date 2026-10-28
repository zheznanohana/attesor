// Sandboxed app checks: container home, app-group container, UserDefaults suite, temp dir, file write, network-less ops.
import Foundation
let env = ProcessInfo.processInfo.environment
print("sandboxed(APP_SANDBOX_CONTAINER_ID)", env["APP_SANDBOX_CONTAINER_ID"] ?? "nil")
print("home", NSHomeDirectory())
print("tmp", NSTemporaryDirectory())
let g = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: "group.attesor.test")
print("group container", g?.path ?? "nil")
let d = UserDefaults(suiteName: "group.attesor.test"); d?.set(42, forKey: "k"); print("suite defaults", d?.integer(forKey: "k") ?? -1)
let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
try? FileManager.default.createDirectory(at: docs, withIntermediateDirectories: true)
print("docs write", (try? "hi".write(to: docs.appendingPathComponent("attesor.txt"), atomically: true, encoding: .utf8)) != nil, docs.path)
print("read /etc/hosts (should FAIL when sandboxed)", (try? String(contentsOfFile: "/Users/Shared/.attesor-probe", encoding: .utf8)) != nil || FileManager.default.isReadableFile(atPath: NSHomeDirectory().replacingOccurrences(of: "/Library/Containers/attesor.sbapp/Data", with: "") + "/Desktop"))
