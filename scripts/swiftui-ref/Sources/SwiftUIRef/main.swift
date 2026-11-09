import AppKit
import Carbon.HIToolbox

// Usage: SwiftUIRef [--dump-once] [--appearance light|dark]
// Without --dump-once: Ctrl+Option+Shift+Space shows the menu at the cursor.
// Esc, a click or the same keys close it.

let arguments = Array(CommandLine.arguments.dropFirst())
let dumpOnce = arguments.contains("--dump-once")
var appearanceName: String?
if let index = arguments.firstIndex(of: "--appearance") {
    appearanceName = index + 1 < arguments.count ? arguments[index + 1] : nil
    if appearanceName != "light" && appearanceName != "dark" {
        Log.fail("--appearance needs light or dark")
        exit(2)
    }
}

MainActor.assumeIsolated {
    let app = NSApplication.shared
    app.setActivationPolicy(.accessory)
    switch appearanceName {
    case "light": app.appearance = NSAppearance(named: .aqua)
    case "dark": app.appearance = NSAppearance(named: .darkAqua)
    default: break
    }
    guard NSScreen.main != nil else {
        Log.fail("no screen: not running in a WindowServer session")
        exit(3)
    }
    let params = Params.fromEnvironment(ProcessInfo.processInfo.environment)
    let controller = MenuController(params: params)

    if dumpOnce {
        Task { @MainActor in
            let code = await Scenario(controller: controller, params: params).run()
            exit(code)
        }
    } else {
        // Control + Option + Shift + Space.
        let modifiers = controlKey | optionKey | shiftKey
        let ok = HotKeys.shared.register(id: 1, keyCode: 49, modifiers: modifiers) {
            if controller.isOpen {
                controller.hide(reason: "hot key")
            } else {
                controller.show(at: NSEvent.mouseLocation)
                DispatchQueue.main.async { controller.animateIn() }
            }
        }
        guard ok else {
            Log.fail("could not register Ctrl+Option+Shift+Space")
            exit(1)
        }
        Log.write("running; press Ctrl+Option+Shift+Space; \(params)")
    }
    app.run()
}
