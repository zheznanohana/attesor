// winlist [substring|pid ...]: on-screen windows (owner, pid, layer, bounds, alpha) without screen-recording
// permission. With arguments, only windows whose owner name contains one of them or whose pid matches.
import CoreGraphics
import Foundation
let filters = Array(CommandLine.arguments.dropFirst())
let all = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]] ?? []
for w in all {
    let owner = w[kCGWindowOwnerName as String] as? String ?? "?"
    let pid = w[kCGWindowOwnerPID as String] as? Int ?? -1
    if !filters.isEmpty && !filters.contains(where: { owner.contains($0) || String(pid) == $0 }) { continue }
    let b = w[kCGWindowBounds as String] as? [String: Double] ?? [:]
    let onscreen = (w[kCGWindowIsOnscreen as String] as? Bool) ?? false
    print(String(format: "%@ pid=%d layer=%d onscreen=%@ %gx%g at (%g,%g) alpha=%.2f", owner, pid, w[kCGWindowLayer as String] as? Int ?? 0,
                 onscreen ? "yes" : "no", b["Width"] ?? 0, b["Height"] ?? 0, b["X"] ?? 0, b["Y"] ?? 0, w[kCGWindowAlpha as String] as? Double ?? 0))
}
