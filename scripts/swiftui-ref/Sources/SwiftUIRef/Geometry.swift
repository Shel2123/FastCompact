import Foundation

/// The menu geometry of the Rust spike (`MenuGeometry::standard()` and the
/// constants of `spike_ui/view.rs`). Copied by hand, not measured: the
/// reference compares looks and timing, not geometry.
enum Geometry {
    static let size = 240.0
    static let neutral = 28.0
    static let outer = 110.0
    static let gap = 0.02
    static let iconBox = 20.0
    static let slots = 9
    static let cancelSlot = 4

    static let step = 2 * Double.pi / Double(slots)
    /// Cancel points straight down.
    static let rotation = (Double.pi - Double(cancelSlot) * step)
        .truncatingRemainder(dividingBy: 2 * Double.pi)

    /// Clockwise angle from straight up to the middle of `slot`.
    static func center(of slot: Int) -> Double {
        (rotation + Double(slot) * step).truncatingRemainder(dividingBy: 2 * Double.pi)
    }

    /// The slot under a point `dx`, `dy` from the menu center (y up), or nil
    /// in the neutral zone and outside. The move threshold of the real
    /// geometry is not reproduced.
    static func slot(dx: Double, dy: Double) -> Int? {
        let radius = hypot(dx, dy)
        guard radius >= neutral, radius <= outer else { return nil }
        let angle = atan2(dx, dy)
        let shifted = (angle - rotation + step / 2).truncatingRemainder(dividingBy: 2 * Double.pi)
        let positive = shifted < 0 ? shifted + 2 * Double.pi : shifted
        return Int(positive / step) % slots
    }

    /// A point at `radius` and clockwise angle `angle` from straight up, in
    /// SwiftUI coordinates (y down) of a `size` square.
    static func point(radius: Double, angle: Double) -> CGPoint {
        CGPoint(x: size / 2 + radius * sin(angle), y: size / 2 - radius * cos(angle))
    }

    static let symbols = [
        "doc.on.doc", "info.circle", "pencil", "archivebox", "xmark",
        "arrow.up.left.and.arrow.down.right", "photo", "photo.on.rectangle", "folder",
    ]
}
