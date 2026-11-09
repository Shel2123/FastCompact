import AppKit
import QuartzCore

/// Snapshots of the layer tree and of rendered pixels. Every snapshot carries
/// its actual time, not the planned one.
@MainActor
enum LayerDump {
    /// All distinct animations seen in the whole run, for the final summary.
    private(set) static var animationsSeen: Set<String> = []
    /// Layers whose `presentation()` was nil.
    private(set) static var noPresentation: Set<String> = []
    private(set) static var layersSeen = 0

    /// Dumps layers worth looking at: with animations, not at identity, or
    /// whose presentation differs from the model. `full` dumps every layer.
    static func snapshot(label: String, since start: CFTimeInterval, view: NSView, full: Bool) {
        let ms = (CACurrentMediaTime() - start) * 1000
        Log.write(String(format: "-- layers [%@] t=%.1f ms%@", label, ms, full ? " (full tree)" : ""))
        guard let root = view.layer else {
            Log.write("   host view has no layer")
            return
        }
        walk(root, path: "0", depth: 0, full: full)
    }

    private static func walk(_ layer: CALayer, path: String, depth: Int, full: Bool) {
        layersSeen += 1
        let presentation = layer.presentation()
        if presentation == nil { noPresentation.insert("\(type(of: layer)) @\(path)") }
        let animations = describeAnimations(layer)
        let modelT = layer.transform
        let presT = presentation?.transform
        let identity = CATransform3DIsIdentity(modelT)
        let differs =
            presentation.map {
                $0.opacity != layer.opacity || !CATransform3DEqualToTransform($0.transform, modelT)
                    || $0.position != layer.position || $0.bounds != layer.bounds
            } ?? false
        if full || !animations.isEmpty || layer.opacity != 1 || !identity || differs {
            var line = String(repeating: "  ", count: depth + 1)
            line += "\(path) \(type(of: layer)) frame=\(fmt(layer.frame))"
            line += " opacity model=\(f(layer.opacity)) pres=\(presentation.map { f($0.opacity) } ?? "nil")"
            line += " scale model=\(scale(modelT)) pres=\(presT.map(scale) ?? "nil")"
            if presentation == nil { line += " [presentation unavailable]" }
            if !animations.isEmpty { line += " anims=[\(animations.joined(separator: "; "))]" }
            Log.write(line)
        }
        for (index, child) in (layer.sublayers ?? []).enumerated() {
            walk(child, path: "\(path).\(index)", depth: depth + 1, full: full)
        }
    }

    private static func describeAnimations(_ layer: CALayer) -> [String] {
        (layer.animationKeys() ?? []).compactMap { key in
            guard let animation = layer.animation(forKey: key) else { return nil }
            let text = describe(animation, key: key)
            animationsSeen.insert(text)
            return text
        }
    }

    private static func describe(_ animation: CAAnimation, key: String) -> String {
        var parts = ["key=\(key)", "\(type(of: animation))"]
        if let property = animation as? CAPropertyAnimation { parts.append("keyPath=\(property.keyPath ?? "nil")") }
        if let basic = animation as? CABasicAnimation {
            parts.append("from=\(basic.fromValue.map { "\($0)" } ?? "nil")")
            parts.append("to=\(basic.toValue.map { "\($0)" } ?? "nil")")
        }
        parts.append("duration=\(f(Float(animation.duration)))")
        parts.append("begin=\(animation.beginTime)")
        parts.append("speed=\(f(animation.speed))")
        parts.append("removedOnCompletion=\(animation.isRemovedOnCompletion)")
        if let timing = animation.timingFunction { parts.append("timing=\(timing)") }
        if let spring = animation as? CASpringAnimation {
            parts.append("mass=\(spring.mass) stiffness=\(spring.stiffness) damping=\(spring.damping)")
            parts.append("initialVelocity=\(spring.initialVelocity) settling=\(spring.settlingDuration)")
        }
        if let group = animation as? CAAnimationGroup {
            let inner = (group.animations ?? []).map { describe($0, key: "-") }
            parts.append("group=[\(inner.joined(separator: " | "))]")
        }
        return parts.joined(separator: " ")
    }

    /// Renders the view with `cacheDisplay` and reads two pixels of `slot`:
    /// the plain fill (radius 40, below the icon) and the pixel of the icon
    /// box that differs most from the fill. The bitmap has an alpha channel
    /// and no glass behind it, so the alpha is of the layer content alone.
    static func pixels(label: String, since start: CFTimeInterval, view: NSView, slot: Int) {
        let ms = (CACurrentMediaTime() - start) * 1000
        guard let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else {
            Log.write("-- pixels [\(label)] t=\(String(format: "%.1f", ms)) ms: no bitmap, not measured")
            return
        }
        view.cacheDisplay(in: view.bounds, to: rep)
        let scale = Double(rep.pixelsWide) / Double(view.bounds.width)
        func pixel(_ point: CGPoint) -> NSColor? {
            rep.colorAt(x: Int(point.x * scale), y: Int(point.y * scale))
        }
        let angle = Geometry.center(of: slot)
        let fillPoint = Geometry.point(radius: 40, angle: angle)
        let fill = pixel(fillPoint)
        var best: (NSColor, Double)?
        let iconCenter = Geometry.point(
            radius: (Geometry.neutral + Geometry.outer) / 2, angle: angle)
        let half = Geometry.iconBox / 2
        if let fill = fill?.usingColorSpace(.sRGB) {
            var y = iconCenter.y - half
            while y <= iconCenter.y + half {
                var x = iconCenter.x - half
                while x <= iconCenter.x + half {
                    if let c = pixel(CGPoint(x: x, y: y))?.usingColorSpace(.sRGB) {
                        let d = abs(c.redComponent - fill.redComponent)
                            + abs(c.greenComponent - fill.greenComponent)
                            + abs(c.blueComponent - fill.blueComponent)
                            + abs(c.alphaComponent - fill.alphaComponent)
                        if d > (best?.1 ?? -1) { best = (c, d) }
                    }
                    x += 1 / scale
                }
                y += 1 / scale
            }
        }
        Log.write(String(format: "-- pixels [%@] t=%.1f ms slot %d: fill %@ | icon box most different %@ | bitmap space %@",
                         label, ms, slot, rgba(fill), rgba(best?.0), rep.colorSpace.localizedName ?? "?"))
    }

    static func summary() {
        Log.write("== layer summary: \(layersSeen) layer visits")
        if animationsSeen.isEmpty {
            Log.write("== CAAnimation: none seen on any layer (SwiftUI drew the motion itself or the layers are not exposed)")
        } else {
            Log.write("== CAAnimation seen (\(animationsSeen.count)):")
            for a in animationsSeen.sorted() { Log.write("   \(a)") }
        }
        if noPresentation.isEmpty {
            Log.write("== presentation() was available on every visited layer")
        } else {
            Log.write("== presentation() was nil on \(noPresentation.count) layers (limitation): \(noPresentation.sorted().prefix(5))")
        }
    }

    private static func scale(_ t: CATransform3D) -> String { "(\(f(Float(t.m11))),\(f(Float(t.m22))))" }
    private static func f(_ v: Float) -> String { String(format: "%.3f", v) }
    private static func fmt(_ r: CGRect) -> String {
        String(format: "(%.1f,%.1f %.1fx%.1f)", r.origin.x, r.origin.y, r.width, r.height)
    }
    private static func rgba(_ color: NSColor?) -> String {
        guard let c = color?.usingColorSpace(.sRGB) else { return "nil" }
        return String(format: "sRGB(%.3f,%.3f,%.3f,a=%.3f)", c.redComponent, c.greenComponent, c.blueComponent, c.alphaComponent)
    }
}
