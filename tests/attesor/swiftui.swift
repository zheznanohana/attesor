import SwiftUI
struct V: View { @State var n = 0
  var body: some View { VStack { Text("Attesor SwiftUI \(n)"); Button("+") { n += 1 } }.frame(width: 300, height: 200)
    .task { for _ in 0..<5 { try? await Task.sleep(for: .milliseconds(200)); n += 1 }; print("swiftui ticks", n); NSApp.terminate(nil) } } }
@main struct A: App { var body: some Scene { WindowGroup { V() } } }
