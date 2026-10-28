import Foundation
struct P: Codable, Hashable { var name: String; var n: Int }
actor Counter { var v = 0; func add(_ x: Int) { v += x } }
protocol Shape { func area() -> Double }
struct Sq: Shape { let s: Double; func area() -> Double { s * s } }
@main struct Main {
  static func main() async throws {
    let ps = (0..<1000).map { P(name: "p\($0)", n: $0) }
    let data = try JSONEncoder().encode(ps)
    let back = try JSONDecoder().decode([P].self, from: data)
    print("codable", back.count, Set(back).count)
    let c = Counter()
    try await withThrowingTaskGroup(of: Void.self) { g in for i in 0..<100 { g.addTask { await c.add(i) } }; try await g.waitForAll() }
    print("actor sum", await c.v)
    let shapes: [any Shape] = (1...5).map { Sq(s: Double($0)) }
    print("existentials", shapes.map { $0.area() }.reduce(0, +))
    let m = "order 1234 ok".firstMatch(of: try Regex("([0-9]+)"))
    print("regex", m.map { String($0.output[1].substring ?? "") } ?? "nil")
    let fmt = Date(timeIntervalSince1970: 0).formatted(.iso8601)
    print("formatted", fmt)
    print("measurement", Measurement(value: 5, unit: UnitLength.kilometers).converted(to: .miles).value > 3)
    try await Task.sleep(for: .milliseconds(50))
    print("ok")
  }
}
