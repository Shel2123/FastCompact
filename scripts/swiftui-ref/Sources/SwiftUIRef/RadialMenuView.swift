import Observation
import SwiftUI

/// What the view shows. Changed by the controller, always on the main actor.
@MainActor @Observable
final class MenuModel {
    var shown = false
    var hovered: Int?
}

/// The ring segment of one slot, between the neutral zone and the outer
/// radius, drawn as a polyline so that arc direction conventions do not matter.
struct SectorShape: Shape {
    let slot: Int

    func path(in rect: CGRect) -> Path {
        let center = Geometry.center(of: slot)
        let start = center - Geometry.step / 2 + Geometry.gap / 2
        let end = center + Geometry.step / 2 - Geometry.gap / 2
        let inner = Geometry.neutral + 2
        let outer = Geometry.outer - 2
        let segments = 24
        var path = Path()
        for i in 0...segments {
            let angle = start + (end - start) * Double(i) / Double(segments)
            let p = Geometry.point(radius: outer, angle: angle)
            if i == 0 { path.move(to: p) } else { path.addLine(to: p) }
        }
        for i in 0...segments {
            let angle = end - (end - start) * Double(i) / Double(segments)
            path.addLine(to: Geometry.point(radius: inner, angle: angle))
        }
        path.closeSubpath()
        return path
    }
}

/// The radial menu as a SwiftUI developer would write it: system styles for
/// every color, no numbers taken from the Rust palette.
struct RadialMenuView: View {
    let model: MenuModel
    let params: Params

    var body: some View {
        ZStack {
            background
            ForEach(0..<Geometry.slots, id: \.self) { slot in
                SectorShape(slot: slot)
                    .fill(sectorStyle(slot))
                icon(slot)
            }
        }
        .frame(width: Geometry.size, height: Geometry.size)
        .scaleEffect(model.shown ? 1 : 0.92)
        .opacity(model.shown ? 1 : 0)
    }

    @ViewBuilder
    private var background: some View {
        let diameter = Geometry.outer * 2
        switch params.background {
        case .glass:
            Color.clear
                .frame(width: diameter, height: diameter)
                .glassEffect(.regular, in: .circle)
        case .material:
            Circle().fill(.regularMaterial).frame(width: diameter, height: diameter)
        case .plain:
            // Hand-picked, not a system style.
            Circle().fill(Color(white: 0.12).opacity(0.96)).frame(width: diameter, height: diameter)
        }
    }

    private func sectorStyle(_ slot: Int) -> AnyShapeStyle {
        let available = !params.unavailable.contains(slot)
        let highlighted = model.hovered == slot
        switch (highlighted, available) {
        case (true, true):
            return slot == Geometry.cancelSlot ? AnyShapeStyle(Color.red) : AnyShapeStyle(.tint)
        case (true, false):
            return AnyShapeStyle(Color.gray.opacity(0.25))
        case (false, true):
            // The 0.3 is hand-picked.
            return slot == Geometry.cancelSlot
                ? AnyShapeStyle(Color.red.opacity(0.3)) : AnyShapeStyle(.fill.tertiary)
        case (false, false):
            return AnyShapeStyle(.fill.quaternary)
        }
    }

    private func iconStyle(_ slot: Int) -> AnyShapeStyle {
        let available = !params.unavailable.contains(slot)
        if model.hovered == slot, available { return AnyShapeStyle(Color.white) }
        return available ? AnyShapeStyle(.primary) : AnyShapeStyle(.tertiary)
    }

    @ViewBuilder
    private func icon(_ slot: Int) -> some View {
        let name = Geometry.symbols[slot]
        let position = Geometry.point(
            radius: (Geometry.neutral + Geometry.outer) / 2, angle: Geometry.center(of: slot))
        if NSImage(systemSymbolName: name, accessibilityDescription: nil) != nil {
            Image(systemName: name)
                .resizable()
                .scaledToFit()
                .frame(width: Geometry.iconBox, height: Geometry.iconBox)
                .foregroundStyle(iconStyle(slot))
                .position(position)
        } else {
            // Symbol missing: leave the slot empty.
            Color.clear.frame(width: 1, height: 1).position(position)
                .onAppear { Log.write("symbol \(name) not found; slot \(slot) left empty") }
        }
    }
}
