import AppKit
import QuartzCore
import SwiftUI

/// `--dump-once`: drives the menu through a fixed scenario and logs snapshots.
///
/// Stages: appearance, then highlight on and off of slot 1, then of the
/// cancel slot. The highlight is driven by `withAnimation`, not by a hover.
/// Every transition waits for its `completion` (criteria `.removed`, the
/// animation is gone) with a ceiling; a missing completion is reported as
/// "not confirmed" and makes the exit code 2.
@MainActor
final class Scenario {
    private let controller: MenuController
    private let params: Params
    private var unconfirmed = 0

    init(controller: MenuController, params: Params) {
        self.controller = controller
        self.params = params
    }

    func run() async -> Int32 {
        header()
        let screen = NSScreen.main ?? NSScreen.screens[0]
        let point = NSPoint(x: screen.visibleFrame.midX, y: screen.visibleFrame.midY)
        controller.show(at: point)
        // Let the first layout commit with `shown == false`.
        try? await Task.sleep(for: .milliseconds(250))
        LayerDump.snapshot(label: "hidden state", since: CACurrentMediaTime(), view: controller.host, full: true)

        let appearMs = params.durationMs
        await stage(
            "appear", ceiling: appearCeiling(), offsets: [0, 16, 50, 150, appearMs / 2], pixelSlot: nil
        ) { done in self.controller.animateIn(completion: done) }
        try? await Task.sleep(for: .milliseconds(150))
        LayerDump.snapshot(label: "after appear", since: CACurrentMediaTime(), view: controller.host, full: true)

        let highlightCeiling = params.highlightMs / 1000 + 1
        for slot in [1, Geometry.cancelSlot] {
            await stage("highlight on slot \(slot)", ceiling: highlightCeiling, offsets: [0, 10, 35, 60], pixelSlot: slot) { done in
                self.controller.setHovered(slot, completion: done)
            }
            try? await Task.sleep(for: .milliseconds(100))
            await stage("highlight off slot \(slot)", ceiling: highlightCeiling, offsets: [0, 10, 35, 60], pixelSlot: slot) { done in
                self.controller.setHovered(nil, completion: done)
            }
            try? await Task.sleep(for: .milliseconds(100))
        }
        LayerDump.summary()
        controller.hide(reason: "scenario finished")
        if unconfirmed > 0 {
            Log.write("== RESULT: \(unconfirmed) transition(s) not confirmed; exit code 2")
            return 2
        }
        Log.write("== RESULT: every transition confirmed; exit code 0")
        return 0
    }

    private func appearCeiling() -> Double {
        let seconds = params.durationMs / 1000
        if params.anim == .spring {
            return min(Spring(duration: seconds, bounce: params.bounce).settlingDuration + 1, 10)
        }
        return min(seconds + 1, 10)
    }

    /// One transition. `trigger` starts it and must call `done` when the
    /// `withAnimation` completion fires.
    private func stage(
        _ name: String, ceiling: Double, offsets: [Double], pixelSlot: Int?,
        trigger: (@escaping () -> Void) -> Void
    ) async {
        let host = controller.host
        Log.write("== stage: \(name) (ceiling \(String(format: "%.2f", ceiling)) s)")
        let start = CACurrentMediaTime()
        LayerDump.snapshot(label: "\(name) before trigger", since: start, view: host, full: false)
        if let pixelSlot { LayerDump.pixels(label: "\(name) before trigger", since: start, view: host, slot: pixelSlot) }
        var tasks: [Task<Void, Never>] = []
        for offset in offsets {
            tasks.append(Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(offset))
                LayerDump.snapshot(label: "\(name) +\(Int(offset)) ms planned", since: start, view: host, full: false)
                if let pixelSlot {
                    LayerDump.pixels(label: "\(name) +\(Int(offset)) ms planned", since: start, view: host, slot: pixelSlot)
                }
            })
        }
        let box = CompletionBox()
        let completed: Bool = await withCheckedContinuation { continuation in
            box.continuation = continuation
            trigger { box.finish(true, at: CACurrentMediaTime() - start) }
            DispatchQueue.main.asyncAfter(deadline: .now() + ceiling) { box.finish(false, at: ceiling) }
        }
        for task in tasks { await task.value }
        if completed {
            Log.write(String(format: "   %@: completion fired at +%.1f ms", name, box.at * 1000))
            LayerDump.snapshot(label: "\(name) completed", since: start, view: host, full: false)
            if let pixelSlot { LayerDump.pixels(label: "\(name) completed", since: start, view: host, slot: pixelSlot) }
        } else {
            unconfirmed += 1
            Log.write("!! \(name): completion not confirmed within \(String(format: "%.2f", ceiling)) s")
            LayerDump.snapshot(label: "\(name) UNCONFIRMED at ceiling", since: start, view: host, full: false)
            if let pixelSlot { LayerDump.pixels(label: "\(name) UNCONFIRMED at ceiling", since: start, view: host, slot: pixelSlot) }
        }
    }

    // MARK: header

    private func header() {
        let os = ProcessInfo.processInfo.operatingSystemVersionString
        let workspace = NSWorkspace.shared
        let dark = NSApp.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        Log.write("== SwiftUIRef --dump-once")
        Log.write("settings (observed): macOS \(os); appearance \(NSApp.effectiveAppearance.name.rawValue); dark=\(dark)")
        Log.write("settings (observed): reduceTransparency=\(workspace.accessibilityDisplayShouldReduceTransparency) reduceMotion=\(workspace.accessibilityDisplayShouldReduceMotion) increaseContrast=\(workspace.accessibilityDisplayShouldIncreaseContrast)")
        Log.write("settings (observed): backingScaleFactor=\(controller.panel.backingScaleFactor) accent=\(rgba(NSColor.controlAccentColor))")
        Log.write("params: \(params)")
        if params.anim == .spring {
            let spring = Spring(duration: params.durationMs / 1000, bounce: params.bounce)
            Log.write("API Spring(duration: \(params.durationMs / 1000), bounce: \(params.bounce)): mass=\(spring.mass) stiffness=\(spring.stiffness) damping=\(spring.damping) settlingDuration=\(spring.settlingDuration)  [parameters of the chosen model, not a measurement of the rendered motion]")
        }
        if params.anim == .spring {
            // The object the Rust spike creates with the same two numbers.
            let core = CASpringAnimation(perceptualDuration: params.durationMs / 1000, bounce: params.bounce)
            Log.write("API CASpringAnimation(perceptualDuration: \(params.durationMs / 1000), bounce: \(params.bounce)): mass=\(core.mass) stiffness=\(core.stiffness) damping=\(core.damping) settlingDuration=\(core.settlingDuration)  [parameters of the chosen model, not a measurement]")
        }
        var env = EnvironmentValues()
        env.colorScheme = dark ? .dark : .light
        for (name, color) in [("accentColor", Color.accentColor), ("primary", Color.primary), ("red", Color.red)] {
            let resolved = color.resolve(in: env)
            let srgb = NSColor(cgColor: resolved.cgColor)
            Log.write("API Color.\(name).resolve(\(dark ? "dark" : "light")): linear extended sRGB (\(resolved.linearRed), \(resolved.linearGreen), \(resolved.linearBlue), a=\(resolved.opacity)); as sRGB \(rgba(srgb))")
        }
    }

    private func rgba(_ color: NSColor?) -> String {
        guard let c = color?.usingColorSpace(.sRGB) else { return "nil" }
        return String(format: "sRGB(%.3f,%.3f,%.3f,a=%.3f)", c.redComponent, c.greenComponent, c.blueComponent, c.alphaComponent)
    }
}

/// Resumes a continuation exactly once, whichever of completion and
/// ceiling comes first.
@MainActor
private final class CompletionBox {
    var continuation: CheckedContinuation<Bool, Never>?
    var at = 0.0

    func finish(_ completed: Bool, at seconds: Double) {
        guard let continuation else { return }
        self.continuation = nil
        at = seconds
        continuation.resume(returning: completed)
    }
}
