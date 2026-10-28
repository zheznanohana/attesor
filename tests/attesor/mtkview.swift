// SwiftUI window hosting an MTKView that clears to a cycling colour; quits after FRAMES draws.
import SwiftUI
import MetalKit
final class R: NSObject, MTKViewDelegate {
  let q: MTLCommandQueue; var n = 0; let limit = Int(ProcessInfo.processInfo.environment["FRAMES"] ?? "120")!
  init(_ d: MTLDevice) { q = d.makeCommandQueue()! }
  func mtkView(_ v: MTKView, drawableSizeWillChange s: CGSize) {}
  func draw(in v: MTKView) {
    guard let p = v.currentRenderPassDescriptor, let d = v.currentDrawable, let cb = q.makeCommandBuffer() else { print("no drawable"); return }
    let t = Double(n % 120) / 120; p.colorAttachments[0].clearColor = MTLClearColor(red: 0.1, green: t, blue: 1 - t, alpha: 1)
    cb.makeRenderCommandEncoder(descriptor: p)!.endEncoding(); cb.present(d); cb.commit(); n += 1
    if n == limit { print("mtkview frames", n); DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { NSApp.terminate(nil) } }
  }
}
struct MV: NSViewRepresentable {
  func makeNSView(context: Context) -> MTKView { let v = MTKView(); v.device = MTLCreateSystemDefaultDevice(); print("device", v.device?.name ?? "nil")
    v.delegate = context.coordinator; v.preferredFramesPerSecond = 60; return v }
  func updateNSView(_ v: MTKView, context: Context) {}
  func makeCoordinator() -> R { R(MTLCreateSystemDefaultDevice()!) }
}
@main struct A: App { var body: some Scene { WindowGroup { MV().frame(width: 320, height: 240) } } }
