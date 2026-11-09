import AppKit
import SwiftUI

/// A panel that never becomes key or main, so the app under the menu keeps
/// the keyboard focus (as in the Rust spike). Esc therefore comes from a
/// Carbon hot key, not from `keyDown`.
final class NonKeyPanel: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

@MainActor
final class MenuController {
    private static let escapeHotKeyID: UInt32 = 2

    let params: Params
    let model = MenuModel()
    let panel: NonKeyPanel
    let host: NSHostingView<RadialMenuView>
    private(set) var center = NSPoint.zero
    private(set) var isOpen = false
    private var monitor: Any?

    init(params: Params) {
        self.params = params
        let frame = NSRect(x: 0, y: 0, width: Geometry.size, height: Geometry.size)
        panel = NonKeyPanel(
            contentRect: frame, styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered, defer: false)
        panel.isReleasedWhenClosed = false
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.level = .popUpMenu
        // Mouse events pass through to the app below; the global monitor sees them.
        panel.ignoresMouseEvents = true
        panel.hidesOnDeactivate = false
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient]
        host = NSHostingView(rootView: RadialMenuView(model: model, params: params))
        host.frame = frame
        panel.contentView = host
    }

    var appearAnimation: Animation? {
        let seconds = params.durationMs / 1000
        switch params.anim {
        case .spring: return .spring(duration: seconds, bounce: params.bounce)
        case .ease: return .easeOut(duration: seconds)
        case .none: return nil
        }
    }

    var highlightAnimation: Animation { .easeOut(duration: params.highlightMs / 1000) }

    /// Puts the panel up around `point` (Cocoa coordinates), still invisible
    /// (`shown == false`). The appearance is started by `animateIn`.
    func show(at point: NSPoint) {
        guard !isOpen else { return }
        var origin = NSPoint(x: point.x - Geometry.size / 2, y: point.y - Geometry.size / 2)
        if let screen = NSScreen.screens.first(where: { NSMouseInRect(point, $0.frame, false) }) {
            let visible = screen.visibleFrame
            origin.x = min(max(origin.x, visible.minX), visible.maxX - Geometry.size)
            origin.y = min(max(origin.y, visible.minY), visible.maxY - Geometry.size)
        }
        center = NSPoint(x: origin.x + Geometry.size / 2, y: origin.y + Geometry.size / 2)
        withoutAnimation {
            model.shown = false
            model.hovered = nil
        }
        panel.setFrameOrigin(origin)
        panel.orderFrontRegardless()
        isOpen = true
        installInputHandlers()
        Log.write("shown at \(center); frontmost app: \(frontmost())")
    }

    func animateIn(completion: (() -> Void)? = nil) {
        if let completion {
            withAnimation(appearAnimation, completionCriteria: .removed) {
                model.shown = true
            } completion: {
                completion()
            }
        } else {
            withAnimation(appearAnimation) { model.shown = true }
        }
    }

    func setHovered(_ slot: Int?, completion: (() -> Void)? = nil) {
        if let completion {
            withAnimation(highlightAnimation, completionCriteria: .removed) {
                model.hovered = slot
            } completion: {
                completion()
            }
        } else {
            withAnimation(highlightAnimation) { model.hovered = slot }
        }
    }

    func hide(reason: String) {
        guard isOpen else { return }
        isOpen = false
        removeInputHandlers()
        panel.orderOut(nil)
        withoutAnimation {
            model.shown = false
            model.hovered = nil
        }
        Log.write("closed: \(reason); frontmost app: \(frontmost())")
    }

    private func withoutAnimation(_ body: () -> Void) {
        var transaction = Transaction()
        transaction.disablesAnimations = true
        withTransaction(transaction, body)
    }

    private func frontmost() -> String {
        NSWorkspace.shared.frontmostApplication?.localizedName ?? "none"
    }

    /// Only while the menu is open: a global monitor for the pointer and
    /// clicks, and Esc as a hot key. Nothing runs while the menu is hidden.
    private func installInputHandlers() {
        monitor = NSEvent.addGlobalMonitorForEvents(
            matching: [.mouseMoved, .leftMouseDown, .rightMouseDown]
        ) { [weak self] event in
            let type = event.type
            MainActor.assumeIsolated { self?.handle(type) }
        }
        if monitor == nil { Log.write("global monitor was not installed") }
        let ok = HotKeys.shared.register(id: Self.escapeHotKeyID, keyCode: 53, modifiers: 0) {
            [weak self] in self?.hide(reason: "Esc")
        }
        if !ok { Log.write("Esc hot key was not registered; close with a click") }
    }

    private func removeInputHandlers() {
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
        HotKeys.shared.unregister(id: Self.escapeHotKeyID)
    }

    private func handle(_ type: NSEvent.EventType) {
        guard isOpen else { return }
        switch type {
        case .mouseMoved:
            let p = NSEvent.mouseLocation
            let slot = Geometry.slot(dx: p.x - center.x, dy: p.y - center.y)
            if slot != model.hovered { setHovered(slot) }
        case .leftMouseDown, .rightMouseDown:
            hide(reason: "click")
        default:
            break
        }
    }
}
