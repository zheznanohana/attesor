import CoreGraphics
let l = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as! [[String: Any]]
for w in l { let o = w[kCGWindowOwnerName as String] as? String ?? ""; if o.contains("metallayer") || o.contains("maclator") { print(o, w[kCGWindowBounds as String] ?? "", "layer", w[kCGWindowLayer as String] ?? "", "alpha", w[kCGWindowAlpha as String] ?? "") } }
print("windows total", l.count)
