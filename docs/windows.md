# Orbit for Windows

Build on Windows with Rust 1.97 or newer and the MSVC C++ build tools.

```powershell
cargo build --locked --release -p orbit-desktop
.\target\release\orbit.exe
```

1. Select one file or folder in File Explorer.
2. Hold Ctrl+Alt+Space to open the radial menu at the pointer.
3. Move toward Copy path, Info, or Reveal and release the shortcut. The
   direction from the menu center chooses, so the pointer may go past the
   ring. Info shows the path, item type and file size. Reveal opens the
   parent folder and selects the item.
4. Press Esc, release in the center, or release toward Cancel (straight
   down) to dismiss the menu. Use the Orbit tray icon to exit the app.

Rename, ZIP, resize, and image conversion are dimmed on Windows and do
nothing when chosen. The menu works in Explorer's file list.

## Files

- Log: `%LOCALAPPDATA%\Orbit\Logs\orbit.log`. A debug build also logs to the
  console; a release build has no console.
- Instance lock: `%LOCALAPPDATA%\Orbit\instance.lock`. A second start shows
  "Orbit is already running" and exits; the first one keeps running.
- A startup failure, such as Ctrl+Alt+Space taken by another program, is
  shown in a message box and written to the log.

## Display scaling

`orbit.exe` embeds `crates/orbit-desktop/orbit.exe.manifest`, which makes it
Per-Monitor v2 DPI aware (MSVC builds; `build.rs` passes it to the linker).
Without the manifest Orbit asks for the mode at startup. The log line
`DPI awareness mode=...` shows the mode in effect.

A gesture keeps the scale of the monitor it starts on. If the scale of a
monitor changes while the menu or Info is shown, the window closes and the
gesture is cancelled.

## Known limitations

- Ctrl+Alt is AltGr on many European keyboard layouts, so Ctrl+Alt+Space can
  clash with a character there.
- The adapter only reads the state of the keys. Alt and Esc also reach
  Explorer, so releasing Alt may show Explorer's key tips.
- Info shows no size for folders, unlike macOS.
- The Rename form uses only the universal name rules. Windows rules
  (`<>:"/\|?*`, `CON`, `NUL` and the other reserved names, a trailing dot or
  space) are needed before Rename is enabled.

## Manual check

Run on Windows:

1. `cargo test --locked --workspace --all-features`, including the
   `orbit-windows` test of a failed gesture timer followed by a working
   gesture.
2. `cargo build --locked --release -p orbit-desktop` and run `orbit.exe`.
   The log says `DPI awareness mode=per-monitor v2`.
3. Start `orbit.exe` again: the message says Orbit is already running, and
   the first instance still opens the menu. Exit from the tray, then start
   Orbit again.
4. Tray: open the tray menu, close it by clicking outside, open it again,
   then exit. Restart Explorer and check that the icon comes back.
5. Use two monitors with different scales, one to the left of or above the
   main one. The menu and Info are sharp and in the right place on both.
   Start a gesture on one monitor, cross onto the other and release: the
   highlighted slot is the one that runs. Change a monitor's scale while
   Info is shown: Info closes.
6. Release Alt first, and in another try release it last. The menu works
   both ways; note whether Explorer shows its key tips.
7. Try a file with a Unicode name, a folder, an empty and a multiple
   selection, and a selection near a screen edge. Check that the menu
   leaves Explorer focused, dimmed actions do nothing, Esc cancels the
   gesture, and Copy path pastes the complete path.
8. Move the pointer far past the ring toward Copy path: Copy path is
   highlighted and the release copies the path. Far straight down, Cancel is
   highlighted and the release does nothing. Open the menu at a screen
   corner and move the pointer a little: the slot highlighted is the one the
   pointer points to from the visible menu center, and the release runs that
   slot.
