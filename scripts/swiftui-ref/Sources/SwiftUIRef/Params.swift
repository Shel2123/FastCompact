import Foundation

/// Settings read from the environment, with range checks. A value that is
/// missing, not a number or out of range falls back to the default and the
/// fallback is logged.
struct Params {
    enum Background: String { case glass, material, plain }
    enum Anim: String { case spring, ease, none }

    var background: Background = .glass
    var anim: Anim = .spring
    var durationMs = 700.0
    var bounce = 0.5
    var highlightMs = 70.0
    var unavailable: Set<Int> = []

    static let durationRange = 0.0...3000.0
    static let highlightRange = 0.0...1000.0

    static func fromEnvironment(_ env: [String: String]) -> Params {
        var p = Params()
        if let raw = env["REF_BACKGROUND"] {
            if let v = Background(rawValue: raw) { p.background = v } else { fellBack("REF_BACKGROUND", raw, "glass") }
        }
        if let raw = env["REF_ANIM"] {
            if let v = Anim(rawValue: raw) { p.anim = v } else { fellBack("REF_ANIM", raw, "spring") }
        }
        if let raw = env["REF_DURATION_MS"] {
            // Exclusive lower bound: zero duration is not an animation.
            if let v = Double(raw), v > 0, Self.durationRange.contains(v) { p.durationMs = v } else { fellBack("REF_DURATION_MS", raw, "700 (range (0, 3000])") }
        }
        if let raw = env["REF_BOUNCE"] {
            // bounce = 1 is an undamped spring: it never settles.
            if let v = Double(raw), v > -1, v < 1 { p.bounce = v } else { fellBack("REF_BOUNCE", raw, "0.5 (range (-1, 1))") }
        }
        if let raw = env["REF_HIGHLIGHT_MS"] {
            if let v = Double(raw), v > 0, Self.highlightRange.contains(v) { p.highlightMs = v } else { fellBack("REF_HIGHLIGHT_MS", raw, "70 (range (0, 1000])") }
        }
        if let raw = env["REF_UNAVAILABLE"] {
            let slots = raw.split(separator: ",").compactMap { Int($0.trimmingCharacters(in: .whitespaces)) }
            let valid = slots.filter { (0..<Geometry.slots).contains($0) }
            if valid.count == slots.count, !slots.isEmpty { p.unavailable = Set(valid) } else { fellBack("REF_UNAVAILABLE", raw, "none") }
        }
        return p
    }

    private static func fellBack(_ name: String, _ raw: String, _ fallback: String) {
        Log.write("params: \(name)=\(raw) rejected, using \(fallback)")
    }
}
