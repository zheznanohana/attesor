// badge <app-path> <out.icns>
// Draws the app's own icon with an "arm" badge in the lower right corner and writes an .icns.
import AppKit

let args = CommandLine.arguments
guard args.count == 3 else { FileHandle.standardError.write("usage: badge <app> <out.icns>\n".data(using: .utf8)!); exit(2) }
let appPath = args[1], outPath = args[2]

// Prefer the bundle's own .icns (Finder's icon for an unrunnable app may carry the "prohibited" overlay).
func bundleIcon() -> NSImage? {
    guard let info = NSDictionary(contentsOfFile: appPath + "/Contents/Info.plist"),
          var name = info["CFBundleIconFile"] as? String else { return nil }
    if !name.hasSuffix(".icns") { name += ".icns" }
    return NSImage(contentsOfFile: appPath + "/Contents/Resources/" + name)
}
let base = bundleIcon() ?? NSWorkspace.shared.icon(forFile: appPath)

func render(_ px: Int) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSGraphicsContext.current?.imageInterpolation = .high
    let s = CGFloat(px)
    base.draw(in: NSRect(x: 0, y: 0, width: s, height: s), from: .zero, operation: .sourceOver, fraction: 1)

    // pill badge, lower right
    let w = s * 0.46, h = s * 0.22
    let r = NSRect(x: s - w - s * 0.04, y: s * 0.06, width: w, height: h)
    let pill = NSBezierPath(roundedRect: r, xRadius: h / 2, yRadius: h / 2)
    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow(); shadow.shadowBlurRadius = s * 0.02; shadow.shadowOffset = NSSize(width: 0, height: -s * 0.008)
    shadow.shadowColor = NSColor.black.withAlphaComponent(0.35); shadow.set()
    NSColor(srgbRed: 0.96, green: 0.45, blue: 0.13, alpha: 1).setFill(); pill.fill()
    NSGraphicsContext.restoreGraphicsState()
    NSColor.white.withAlphaComponent(0.9).setStroke(); pill.lineWidth = max(1, s * 0.012); pill.stroke()

    let font = NSFont.systemFont(ofSize: h * 0.62, weight: .heavy)
    let text = NSAttributedString(string: "arm", attributes: [.font: font, .foregroundColor: NSColor.white])
    let ts = text.size()
    text.draw(at: NSPoint(x: r.midX - ts.width / 2, y: r.midY - ts.height / 2))
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

let fm = FileManager.default
let set = NSTemporaryDirectory() + "badge-\(getpid()).iconset"
try? fm.removeItem(atPath: set)
try! fm.createDirectory(atPath: set, withIntermediateDirectories: true)
for pt in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(pt)x\(pt).png" : "icon_\(pt)x\(pt)@2x.png"
        try! render(pt * scale).representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: set + "/" + name))
    }
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", set, "-o", outPath]
try! p.run(); p.waitUntilExit()
try? fm.removeItem(atPath: set)
exit(p.terminationStatus)
