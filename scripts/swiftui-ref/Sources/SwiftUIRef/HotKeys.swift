import Carbon.HIToolbox
import Foundation

/// Carbon hot keys: they need no Accessibility permission and never make the
/// panel a key window.
@MainActor
final class HotKeys {
    static let shared = HotKeys()

    private var actions: [UInt32: () -> Void] = [:]
    private var refs: [UInt32: EventHotKeyRef] = [:]
    private var handlerInstalled = false

    /// Registers a key. Returns false when the system refused it.
    func register(id: UInt32, keyCode: Int, modifiers: Int, action: @escaping () -> Void) -> Bool {
        installHandler()
        var ref: EventHotKeyRef?
        let hotKeyID = EventHotKeyID(signature: 0x4F52_4246, id: id)
        let status = RegisterEventHotKey(
            UInt32(keyCode), UInt32(modifiers), hotKeyID, GetApplicationEventTarget(), 0, &ref)
        guard status == noErr, let ref else {
            Log.write("RegisterEventHotKey id \(id) failed: OSStatus \(status)")
            return false
        }
        refs[id] = ref
        actions[id] = action
        return true
    }

    func unregister(id: UInt32) {
        if let ref = refs.removeValue(forKey: id) { UnregisterEventHotKey(ref) }
        actions.removeValue(forKey: id)
    }

    fileprivate func fire(_ id: UInt32) {
        actions[id]?()
    }

    private func installHandler() {
        guard !handlerInstalled else { return }
        var spec = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        let handler: EventHandlerUPP = { _, event, _ in
            var hotKeyID = EventHotKeyID()
            let status = GetEventParameter(
                event, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID),
                nil, MemoryLayout<EventHotKeyID>.size, nil, &hotKeyID)
            if status == noErr {
                let id = hotKeyID.id
                DispatchQueue.main.async { MainActor.assumeIsolated { HotKeys.shared.fire(id) } }
            }
            return noErr
        }
        let status = InstallEventHandler(GetApplicationEventTarget(), handler, 1, &spec, nil, nil)
        if status == noErr {
            handlerInstalled = true
        } else {
            Log.write("InstallEventHandler failed: OSStatus \(status)")
        }
    }
}
